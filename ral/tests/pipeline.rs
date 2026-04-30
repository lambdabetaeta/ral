// Integration tests for pipeline mechanics: process groups, signal routing,
// broken pipes, concurrent spawned pipelines, and output correctness.
//
// All tests run ral as a subprocess.  Signal relay and tcsetpgrp are
// only active in the interactive shell (is_interactive=true); these tests
// exercise the batch-mode plumbing — process group setup, pipe wiring,
// exit-status propagation — which is shared with the interactive shell.
//
// Unix-only: these tests rely on Unix commands (/bin/echo, grep, cat, yes,
// head, wc) and Unix process-group / signal semantics.
#![cfg(unix)]

mod common;

use common::{Output, ral_bin};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(1);

fn fresh_tmp_script_path(prefix: &str) -> PathBuf {
    let mut tmp = std::env::temp_dir();
    let pid = std::process::id();
    let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    tmp.push(format!("{prefix}_{pid}_{id}_{nanos}.ral"));
    tmp
}

fn run(script: &str) -> Output {
    run_with_stdin(script, b"")
}

fn run_with_stdin(script: &str, stdin_data: &[u8]) -> Output {
    // Write script to a temp file so ral-run can read it.
    let tmp = fresh_tmp_script_path("ral_test");
    std::fs::write(&tmp, script).unwrap();

    let mut child = Command::new(ral_bin())
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral-run");

    if !stdin_data.is_empty() {
        child.stdin.take().unwrap().write_all(stdin_data).unwrap();
    }

    let out = child.wait_with_output().unwrap();
    std::fs::remove_file(&tmp).ok();

    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

fn run_with_timeout(script: &str, timeout: Duration) -> Option<Output> {
    let tmp = fresh_tmp_script_path("ral_test");
    std::fs::write(&tmp, script).unwrap();

    let mut child = Command::new(ral_bin())
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral-run");

    let start = std::time::Instant::now();
    loop {
        match child.try_wait().unwrap() {
            Some(_) => {
                let out = child.wait_with_output().unwrap();
                std::fs::remove_file(&tmp).ok();
                return Some(Output {
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    status: out.status.code().unwrap_or(1),
                });
            }
            None if start.elapsed() > timeout => {
                child.kill().ok();
                std::fs::remove_file(&tmp).ok();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn run_args_with_timeout(args: &[&str], script: &str, timeout: Duration) -> Option<Output> {
    use std::io::Read;

    let tmp = fresh_tmp_script_path("ral_test");
    std::fs::write(&tmp, script).unwrap();

    let mut child = Command::new(ral_bin());
    child
        .args(args)
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = child.spawn().expect("spawn ral-run");
    let stdout_reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        })
    });
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    let start = std::time::Instant::now();
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                std::fs::remove_file(&tmp).ok();
                return Some(Output {
                    stdout: stdout_reader
                        .and_then(|jh| jh.join().ok())
                        .map(|buf| String::from_utf8_lossy(&buf).into_owned())
                        .unwrap_or_default(),
                    stderr: stderr_reader
                        .and_then(|jh| jh.join().ok())
                        .map(|buf| String::from_utf8_lossy(&buf).into_owned())
                        .unwrap_or_default(),
                    status: status.code().unwrap_or(1),
                });
            }
            None if start.elapsed() > timeout => {
                child.kill().ok();
                let _ = child.wait();
                std::fs::remove_file(&tmp).ok();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

// ── All-external pipelines ───────────────────────────────────────────────────

#[test]
fn external_pipeline_basic_grep() {
    let o = run("/bin/echo hello | grep hello");
    assert_eq!(o.status, 0);
    assert_eq!(o.stdout.trim(), "hello");
}

#[test]
fn external_pipeline_no_match_exits_one() {
    let o = run("/bin/echo hello | grep zzz");
    assert_ne!(o.status, 0);
    assert!(o.stdout.trim().is_empty());
}

#[test]
fn external_pipeline_deep_chain() {
    // Five cat stages — verifies process group setup for a long pipeline.
    let o = run("/bin/echo NEEDLE | cat | cat | cat | cat | grep NEEDLE");
    assert_eq!(o.status, 0);
    assert!(o.stdout.contains("NEEDLE"));
}

#[test]
fn external_pipeline_exit_status_from_last_stage() {
    // false is /bin/false here (ral's `false` boolean is handled differently).
    let o = run("/bin/echo hello | /bin/false");
    // non-zero because /bin/false exits 1
    assert_ne!(o.status, 0);
}

#[test]
fn external_pipeline_argument_errors_are_not_dropped() {
    let o = run("/bin/echo $missing | cat");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("undefined variable"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn audited_external_command_large_stderr_does_not_deadlock() {
    let script = r#"/bin/sh -c 'head -c 131072 /dev/zero >&2'"#;
    let o = run_args_with_timeout(&["--audit"], script, Duration::from_secs(5))
        .expect("audited external command timed out — probable stdout/stderr pipe deadlock");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stderr.contains("\"stderr\""), "stderr: {}", o.stderr);
}

#[test]
fn redirect_stderr_to_stdout_flows_through_pipeline() {
    // Inner block captures stdout (with 2>&1 merging stderr in) as a String
    // via the byte-mode bind capture; from-string is then identity on String.
    let o = run("let s = !{!{/bin/sh -c 'printf out; printf err >&2' 2>&1} | from-string}\necho $s");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "outerr");
}

// ── Stage dispatch parity (handlers, ^name, redirects) ─────────────────────
//
// These regressions cover the rule that pipeline-stage dispatch must match
// `dispatch_by_name`: a `within [handlers: …]` interception of an external
// name must fire even mid-pipeline, `^name` must skip aliases/builtins
// (pipeline included), and stage-level redirects must be honored rather than
// silently dropped.

#[test]
fn pipeline_stage_handler_intercepts_unknown_external() {
    // `mycmd` is not a builtin and (assumedly) not on PATH.  Without the
    // handler-match check in analyze_stage, the pipeline classifies the
    // stage as External and the launcher tries to spawn `mycmd`, failing
    // with ENOENT before the handler can run.
    let o = run(
        "within [handlers: [mycmd-pipeline-test: { /bin/echo handled }]] \
            { mycmd-pipeline-test | cat }",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "handled");
}

#[test]
fn pipeline_stage_caret_external_only_bypasses_builtin() {
    // `echo` is a ral builtin.  `^echo` must reach the external /bin/echo
    // (or equivalent) via PATH, even when used as a pipeline stage.
    let o = run("^echo HELLO | cat");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "HELLO");
}

#[test]
fn pipeline_stage_caret_still_fires_per_name_handler() {
    // dispatch_by_name's rule: per-name handlers fire unconditionally;
    // ^name bypasses alias/builtin lookup but does NOT escape an explicit
    // per-name handler frame.  Pipeline-stage classification must agree
    // with the single-command path — otherwise `^echo X | cat` would
    // bypass the handler when the same call outside a pipeline would
    // honor it.  Locked in via the shared classify_dispatch.
    let o = run(
        "within [handlers: [echo: { /bin/echo via-handler }]] \
            { ^echo IGNORED | cat }",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "via-handler");
}

#[test]
fn pipeline_external_stage_rejects_list_arg_with_hint() {
    // Passing a List as a positional arg to an external stage must error
    // with the same diagnostic exec_external produces — `...$xs` hint.
    // Before the shared resolve_command path, the pipeline launcher would
    // happily stringify the list and hand the (likely garbled) text to
    // execve.
    let o = run("let xs = [1, 2, 3]; /bin/echo hi | /usr/bin/printf $xs");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("cannot pass List"),
        "stderr: {}",
        o.stderr
    );
    assert!(o.stderr.contains("...$"), "hint missing: {}", o.stderr);
}

#[test]
fn mixed_pipeline_first_external_stage_does_not_inherit_tty_stdin() {
    // `cat | from-lines` is a mixed pipeline (cat is external, from-lines
    // is internal).  In an interactive shell with a tty stdin, cat must
    // *not* inherit fd 0 — its pgid is not foregrounded, so reading the
    // tty would SIGTTIN it and ral's pump would hang.
    //
    // This batch-mode test exercises the same code path with non-tty
    // stdin (Stdio::null fed into ral).  The mixed-pipeline stdin route
    // should resolve to Null for the first external stage when there's
    // no upstream pipe, and the pipeline should terminate promptly with
    // an empty result rather than blocking on cat's read.
    let o = run_with_timeout(
        "let xs = !{cat | from-lines}; echo done; echo !{length $xs}",
        Duration::from_secs(5),
    )
    .expect("mixed-pipeline first external stage hung — likely inherited stdin");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("done"), "stdout: {}", o.stdout);
}

#[test]
fn pipeline_stage_redirect_to_file_is_honored() {
    // `cmd > file | next` must redirect cmd's stdout to file (not into the
    // pipe).  Bash's behavior: the pipe gets EOF; the file gets the bytes.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("ral_pipe_redir_{pid}_{nanos}.txt"));
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run(&format!(
        "/bin/echo redirected > '{path_str}' | cat\n/bin/echo done\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("redirected"),
        "file did not receive redirected bytes"
    );
}

// ── Broken pipe ──────────────────────────────────────────────────────────────

#[test]
fn broken_pipe_large_producer_small_consumer() {
    // yes generates infinite output; head reads 100 lines and closes the pipe.
    // The pipeline must not hang.
    let o = run_with_timeout("yes MARKER | head -100 | wc -l", Duration::from_secs(5))
        .expect("pipeline timed out — probable broken-pipe deadlock");
    assert_eq!(o.status, 0);
    assert_eq!(o.stdout.trim(), "100");
}

#[test]
fn broken_pipe_very_large_count() {
    let o = run_with_timeout("yes DATA | head -10000 | wc -l", Duration::from_secs(10))
        .expect("pipeline timed out");
    assert_eq!(o.status, 0);
    assert_eq!(o.stdout.trim(), "10000");
}

// ── Concurrent spawned pipelines ─────────────────────────────────────────────

#[test]
fn spawned_pipelines_run_concurrently() {
    // 8 pipelines spawned at once; each squares a number and cats it.
    // All must complete and produce the right values.  `await` returns a
    // record; the block's stdout sits in `[stdout]` as Bytes, decoded for
    // printing.
    let script = r#"
let handles = !{ map { |i|
    let v = $[$i * $i]
    !{spawn { /bin/echo $v | cat }}
} [1, 2, 3, 4, 5, 6, 7, 8] }
!{ map { |h|
    let r = await $h
    echo !{to-bytes $r[stdout] | from-string}
} $handles }
echo done
"#;
    let o = run(script);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("done"));
    // All squares must appear somewhere in output.
    for (i, sq) in [
        (1, 1),
        (2, 4),
        (3, 9),
        (4, 16),
        (5, 25),
        (6, 36),
        (7, 49),
        (8, 64),
    ] {
        assert!(
            o.stdout.contains(&sq.to_string()),
            "missing {i}^2 = {sq} in output:\n{}",
            o.stdout
        );
    }
}

#[test]
fn spawned_pipeline_result_is_awaitable() {
    let script = r#"
let h = !{spawn { /bin/echo 42 | cat }}
let r = await $h
echo !{to-bytes $r[stdout] | from-string}
"#;
    let o = run(script);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("42"));
}

// ── Mixed pipeline output ────────────────────────────────────────────────────

#[test]
fn mixed_pipeline_seq_to_wc() {
    // ral's seq is exclusive at the upper bound (like range), so seq 1 21
    // produces [1..20] — 20 elements.  grep -c counts non-empty lines
    // (`to-lines` does not emit a trailing newline, so plain `wc -l` undercounts).
    let o = run("seq 1 21 | to-lines | grep -c .");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let count: u32 = o.stdout.trim().parse().expect("grep -c output");
    assert_eq!(count, 20);
}

#[test]
fn mixed_pipeline_seq_grep() {
    // Filter the newline-joined seq output for a substring.
    let o = run("seq 1 100 | to-lines | grep 42");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("42"));
}

#[test]
fn mixed_pipeline_internal_byte_stage_buffers_output_cleanly() {
    let script = r#"
let lines = !{printf "a\nb\n" | map-lines { |x| return $x } | from-lines}
echo !{length $lines}
echo $lines[0]
echo $lines[1]
"#;
    let o = run(script);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let out: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(out, vec!["2", "a", "b"]);
}

// ── Stress: many sequential pipelines ───────────────────────────────────────

#[test]
fn many_sequential_pipelines_no_leak() {
    // Run 200 external pipelines in sequence.  If file descriptors or process
    // groups leak, this will exhaust them and start failing.
    let script = r#"
let _go = { |n|
    if $[$n <= 0] {} else {
        /bin/echo $n | cat | grep . > /dev/null
        _go $[$n - 1]
    }
}
_go 200
echo done
"#;
    let o = run_with_timeout(script, Duration::from_secs(30))
        .expect("sequential pipeline stress timed out");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("done"));
}

// ── Stopped pipeline children ────────────────────────────────────────────────

#[test]
fn pipeline_self_stopping_child_does_not_hang_ral() {
    // Without WUNTRACED, child.wait() only returns on termination — a
    // SIGTSTP'd child leaves the pipeline stuck and the terminal owned by
    // the stopped pgid.  wait_handling_stop must detect WIFSTOPPED, kill
    // the pgid (no job control), and reap so ral can exit promptly.
    //
    // Drives this by having stage 1 SIGSTOP itself; the entire pipeline
    // pgid then needs to be killed by ral's wait helper.
    let o = run_with_timeout(
        "/bin/sh -c 'kill -STOP $$' | cat",
        Duration::from_secs(5),
    )
    .expect("pipeline hung after child stopped — wait_handling_stop did not fire");
    // Child was SIGKILL'd by ral after SIGSTOP detection — exit is non-zero.
    assert_ne!(o.status, 0, "expected non-zero exit");
}

#[test]
fn pipeline_self_stopping_child_with_pumped_stdout_does_not_hang() {
    // Same shape as the previous test, but here stage 1's stdout is
    // routed through a *pump thread* (because stage 2 is internal —
    // `from-string`).  If `join` waited for the pump before the child,
    // the pump would block forever reading a pipe held open by the
    // stopped child.  Reordering — wait first, then join the drainer —
    // ensures the wait helper kills the pgid, the pipe closes, and the
    // pump returns.
    let o = run_with_timeout(
        "let s = !{/bin/sh -c 'kill -STOP $$' | from-string}; echo done",
        Duration::from_secs(5),
    )
    .expect("pumped-stdout stop hung — drainer joined before wait");
    // Pipeline failure propagates as non-zero status.
    assert_ne!(o.status, 0, "expected non-zero exit");
}

// ── SIGINT kills external child ──────────────────────────────────────────────

#[test]
fn sigint_kills_external_child_in_pipeline() {
    // Spawn ral running a pipeline where an external process (sleep) is
    // the last stage.  Send SIGINT to the ral process group.  It must
    // terminate within a short deadline — not block forever.
    //
    // In batch mode (non-interactive), the relay is not active; SIGINT goes to
    // the ral process itself via the counting handler, which sets the
    // interrupted flag.  The external children got SIG_DFL via pre_exec and
    // will die on SIGINT delivered to their process group via the terminal
    // driver — or, since we are sending to the whole ral pgid, to all of
    // them.
    let mut tmp = std::env::temp_dir();
    tmp.push("ral_sigint_test.ral");
    std::fs::write(&tmp, "/bin/echo start | sleep 60\n").unwrap();

    // Put ral in its own process group so kill(-pid) reaches exactly
    // ral without affecting the cargo test runner's group.
    let mut cmd = Command::new(ral_bin());
    cmd.arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn");

    let pid = child.id() as libc::pid_t;

    // Let the pipeline start.
    std::thread::sleep(Duration::from_millis(100));

    // Send SIGINT to ral's process group.
    unsafe {
        libc::kill(-pid, libc::SIGINT);
    }

    let start = std::time::Instant::now();
    let deadline = Duration::from_secs(3);
    let exited = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if start.elapsed() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !exited {
        child.kill().ok();
    }
    child.wait().ok();
    std::fs::remove_file(&tmp).ok();
    assert!(exited, "ral did not exit after SIGINT within {deadline:?}");
}

// ── Stdin-consuming builtins ─────────────────────────────────────────────────

#[test]
fn parse_json_from_pipeline() {
    // ext→builtin: external echo pipes JSON into from-json.
    let o = run(r#"let d = !{/bin/echo '{"x":42}' | from-json}
echo $d[x]"#);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "42");
}

#[test]
fn parse_json_from_arg() {
    // Decode an in-hand JSON string by piping through to-string | from-json.
    let o = run(r#"let d = !{to-string '{"y":7}' | from-json}
echo $d[y]"#);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "7");
}

#[test]
fn read_lines_from_pipeline() {
    // ext→builtin: count lines produced by an external command.
    let o = run(r#"let ls = !{/bin/echo -e "a
b
c" | from-lines}
echo !{length $ls}"#);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn read_string_from_non_utf8_pipeline_fails() {
    // from-string is strict UTF-8: invalid bytes must produce an error,
    // not silently corrupt the data with replacement characters.
    let o = run("let s = !{/usr/bin/printf '\\377\\376A' | from-string}\necho !{length $s}");
    assert_ne!(o.status, 0, "expected failure on non-UTF-8 input");
    assert!(
        o.stderr
            .contains("from-string: input is not valid UTF-8"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn read_json_from_non_utf8_pipeline_fails() {
    // json is strict: invalid UTF-8 input should fail instead of lossy-decoding.
    let o = run_with_stdin("from-json", &[0xff, 0xfe, b'A']);
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("from-json: input is not valid UTF-8"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn read_json_from_valid_utf8_pipeline_still_works() {
    let o = run("let d = !{/bin/echo '{\"ok\":true}' | from-json}\necho $d[ok]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "true");
}

#[test]
fn to_bytes_roundtrips_through_from_bytes() {
    // to-bytes returns Value::Bytes (not a list of ints).
    // Verify the roundtrip via length and string decoding (pure ASCII input).
    let o = run(
        "let bs = !{return [65, 66, 67] | to-bytes | from-bytes}\necho !{length $bs}\nlet txt = !{to-bytes $bs | from-string}\necho $txt",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(lines, vec!["3", "ABC"]);
}

#[test]
fn to_bytes_rejects_out_of_range_values() {
    let o = run("return [256] | to-bytes");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("to-bytes: byte at index 0 out of range"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn to_bytes_rejects_non_int_values() {
    let o = run("return ['x'] | to-bytes");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("to-bytes: expected Int at index 0"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn ext_command_result_is_string_not_bytes() {
    // External command captures decode to String, one trailing \n stripped.
    let o = run("let x = printf 'hello\\n'\necho $x");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hello");
}

#[test]
fn ext_command_single_newline_stripped() {
    // One trailing \n stripped; a second \n is preserved.
    let o = run("let x = printf 'a\\n\\n'\necho $x");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "a\n\n");
}

#[test]
fn ext_command_non_utf8_gives_named_error() {
    // Invalid UTF-8 output from an external command is a runtime error.
    let o = run("let x = /usr/bin/printf '\\377'");
    assert_ne!(o.status, 0, "expected failure on non-UTF-8 output");
    assert!(
        o.stderr.contains("returned bytes that are not valid UTF-8"),
        "stderr: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("from-bytes"),
        "hint missing: {}",
        o.stderr
    );
}

#[test]
fn read_lines_from_stdin() {
    let o = run_with_stdin(
        "let ls = !{from-lines}\necho !{length $ls}",
        b"one\ntwo\nthree\n",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn fold_lines_from_pipeline() {
    // Count lines using fold-lines with an integer accumulator.
    let o = run(
        r#"let n = !{/bin/echo -e "a\nb\nc" | fold-lines { |acc _| return $[$acc + 1] } 0}
echo $n"#,
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn fold_lines_from_stdin() {
    let o = run_with_stdin(
        "let n = !{fold-lines { |acc _| return $[$acc + 1] } 0}\necho $n",
        b"x\ny\n",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "2");
}

// ── Internal→internal composition over byte pipelines ─────────────────────────

#[test]
fn internal_to_internal_map_identity() {
    // ext → from-lines (internal) → map { |x| x } (internal)
    // Using map with identity (just returning x) lets us check the element count.
    let o = run(r#"let result = !{/bin/echo -e "a
b
c" | from-lines | map { |x| return $x }}
echo !{length $result}"#);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn internal_to_internal_map_matches_direct() {
    // `find . -name "*.rs" | from-lines` and
    // `find . -name "*.rs" | from-lines | map { |x| return $x }`
    // must produce the same set of file paths.
    //
    // Running inside the ral process's working directory (workspace root).
    let o = run(r#"
let direct  = find . -name "*.rs" -not -path "./target/*" | from-lines
let via_map = find . -name "*.rs" -not -path "./target/*" | from-lines | map { |x| return $x }
echo !{length $direct}
echo !{length $via_map}
"#);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<&str> = o.stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected two count lines, got: {:?}", lines);
    assert_eq!(
        lines[0], lines[1],
        "direct len {} != via_map len {}",
        lines[0], lines[1]
    );
    let count: usize = lines[0].parse().expect("count");
    assert!(count > 0, "no .rs files found");
}

// ── Sandbox IPC subprocess stdio routing ────────────────────────────────
//
// These three tests verify that the grant IPC subprocess correctly handles
// all three stdio configurations from the parent:
//
//   1. stdout → Pipe  (grant body is a pipeline stage)
//   2. stdout → capture via let (grant body produces a value via from-X)
//   3. stdin  → pipe reader (grant body has upstream pipeline input)
//
// Without the sandboxing feature the IPC subprocess is not spawned and the
// tests exercise the in-process fallback path; with the feature the same
// scripts exercise the new configure_subprocess_stdio wiring.

// The IPC subprocess enters the platform OS sandbox.  Linux often lacks
// unprivileged user namespaces in containers, and macOS Seatbelt can be
// unavailable under some test runners, so probe once and skip IPC plumbing
// tests when the kernel sandbox cannot be entered.
fn sandbox_functional() -> bool {
    #[cfg(target_os = "linux")]
    {
        return bwrap_functional();
    }
    #[cfg(target_os = "macos")]
    {
        return macos_sandbox_functional();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        true
    }
}

#[cfg(target_os = "linux")]
fn bwrap_functional() -> bool {
    // Dynamic `/usr/bin/true` needs `/lib` for ld.so; on modern Debian
    // `/bin` is a symlink to `/usr/bin`, so binding `/usr` and `/lib`
    // is the minimum to actually execute inside the new namespace.
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/lib",
            "/lib",
            "--",
            "/usr/bin/true",
        ])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn macos_sandbox_functional() -> bool {
    Command::new(ral_bin())
        .args([
            "--sandbox-projection",
            r#"{"fs":{"read_prefixes":[],"write_prefixes":[]},"connect_prefixes":null,"bind_prefixes":null}"#,
            "--norc",
            "-c",
            "return unit",
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn grant_fs_pipeline_stdout_flows() {
    // Grant is a pipeline stage: its stdout goes to a Pipe sink.
    // configure_subprocess_stdio must clone the pipe writer and hand it to
    // the IPC subprocess; cat on the right side must receive the output.
    if !sandbox_functional() {
        return;
    }
    let o =
        run("grant [exec: ['/bin/echo': []], fs: [read: ['/tmp']]] { /bin/echo sandboxed } | cat");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "sandboxed");
}

#[test]
fn grant_fs_capture_returns_output() {
    // Grant body result is captured in a let binding and echoed.
    // Tests that output produced inside the grant (via from-lines) reaches the
    // parent — both via the in-process fallback and the IPC subprocess path.
    if !sandbox_functional() {
        return;
    }
    let o = run(
        "let x = grant [exec: ['/bin/echo': []], fs: [read: ['/tmp']]] { /bin/echo captured | from-lines }; echo $x[0]",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "captured");
}

#[test]
fn grant_fs_pipeline_stdin_forwarded() {
    // An upstream stage pipes data into the grant body.
    // configure_subprocess_stdio must move the pipe reader into the IPC
    // subprocess's stdin so that the body reads the upstream data.
    // Uses from-lines (ral builtin) so that pipe_stdin is consumed directly
    // rather than through an inner pipeline.
    if !sandbox_functional() {
        return;
    }
    let o = run("let x = /bin/echo piped | grant [fs: [read: ['/tmp']]] { from-lines }; echo $x[0]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "piped");
}

#[test]
fn grant_exec_bare_name_denied_when_scoped_path_rebinds_command() {
    if !sandbox_functional() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fake_git = dir.path().join("git");
    std::fs::write(&fake_git, "#!/bin/sh\necho spoofed\n").unwrap();
    let mut perms = std::fs::metadata(&fake_git).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_git, perms).unwrap();

    // fs:read is needed so the bwrap sandbox binds the tempdir; without it
    // PATH lookup inside the sandbox can't even see the spoofed git, and we
    // get "command not found" instead of the expected denial.  /tmp is
    // tmpfs'd by bwrap by default — only explicit binds make tempfile paths
    // reachable inside the IPC subprocess.  The grant fs:read clause is
    // semantically orthogonal to the exec/PATH-spoofing check this test
    // exercises.
    let script = format!(
        "within [env: [PATH: '{0}']] {{ grant [exec: [git: []], fs: [read: ['{0}']]] {{ git }} }}",
        dir.path().to_string_lossy()
    );
    let o = run(&script);
    assert_eq!(o.status, 1, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    assert!(o.stderr.contains("denied by active grant"));
}

#[test]
fn grant_exec_explicit_path_allows_scoped_path_command() {
    if !sandbox_functional() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fake_git = dir.path().join("git");
    std::fs::write(&fake_git, "#!/bin/sh\necho spoofed\n").unwrap();
    let mut perms = std::fs::metadata(&fake_git).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_git, perms).unwrap();

    // See sibling test: fs:read for the tempdir is required so the bwrap
    // sandbox can actually exec the spoofed git from /tmp/...
    let script = format!(
        "within [env: [PATH: '{0}']] {{ grant [exec: ['{1}': []], fs: [read: ['{0}']]] {{ git }} }}",
        dir.path().to_string_lossy(),
        fake_git.to_string_lossy()
    );
    let o = run(&script);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "spoofed");
}

#[test]
fn pipeline_external_stage_expands_empty_spread_to_zero_args() {
    // Regression: external pipeline stages used to stringify each raw Val,
    // so `...$xs` with an empty list became a single "" argv entry — and
    // trailing `""` confused commands like fzf ("unknown option:").
    // analyze_stage must expand spreads the same way eval_call_args does.
    let o = run("let e = []; echo hi | /usr/bin/printf '[%s]\\n' --flag '' ...$e");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "[--flag]\n[]\n");
}

#[test]
fn pipeline_external_stage_expands_nonempty_spread() {
    let o = run("let e = ['-n', 'hello']; echo hi | /usr/bin/printf '[%s]\\n' --flag ...$e");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "[--flag]\n[-n]\n[hello]\n");
}

#[test]
fn grant_fs_write_through_symlinked_prefix_to_nonexistent_target() {
    // Regression: resolve_grant_path must canonicalize the grant prefix and
    // the target path consistently.  On macOS `/tmp -> /private/tmp`, so
    // `canonicalize('/tmp/')` returns `/private/tmp` — but `canonicalize` of
    // a non-existent file returns ENOENT, leaving the target unresolved.
    // `starts_with` then fails and the write is denied.  The fix walks up
    // to the longest existing ancestor and re-appends the tail, so the
    // target resolves through the symlink too.
    if !sandbox_functional() {
        return;
    }
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let real = std::env::temp_dir().join(format!("ral_grant_real_{pid}_{nanos}"));
    let link = std::env::temp_dir().join(format!("ral_grant_link_{pid}_{nanos}"));
    std::fs::create_dir_all(&real).unwrap();
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let target_via_link = link.join("new-file.log");
    let target_via_real = real.join("new-file.log");
    assert!(
        !target_via_real.exists(),
        "precondition: target must not exist"
    );

    let grant_prefix = format!("{}/", link.display());
    let script = format!(
        "grant [fs: [write: ['{prefix}']]] {{ to-string 'hi' > '{path}' }}; printf done\\n",
        prefix = grant_prefix,
        path = target_via_link.display(),
    );
    let o = run(&script);

    // Cleanup before assertions so a failure doesn't leak the symlink.
    let wrote = target_via_real.exists();
    let _ = std::fs::remove_file(&target_via_real);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&real);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        !o.stderr.contains("fs write denied by grant"),
        "write was denied: {}",
        o.stderr,
    );
    assert!(
        wrote,
        "redirect did not create the file under the symlink target"
    );
}

// ── Capture semantics ────────────────────────────────────────────────────
//
// These tests verify the principle: `let` binds the return value of its RHS.
// For byte-output commands the return value is the decoded String of the last
// command's bytes.  For value-returning commands the return value is bound
// directly.  Block and higher-order function cases follow the same rules.

#[test]
fn block_return_captures_only_last_command() {
    // The block's return value is the last command's bytes decoded — not a
    // concatenation of all commands' output.  Non-final bytes flush to stdout
    // (the outer stream), so the final `echo` of `$x` shows `[three]` last.
    let o = run("let x = !{ echo one; echo two; echo three }\necho \"[$x]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let last = o.stdout.trim().lines().last().unwrap_or("");
    assert_eq!(last, "[three]", "full stdout: {:?}", o.stdout);
}

#[test]
fn block_non_final_bytes_reach_terminal() {
    // Non-final commands in a captured block flush their bytes to the outer
    // stdout (the terminal) so side-effects are visible.
    let o = run("let x = !{ echo log; echo result }\necho \"x=$x\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        o.stdout.contains("log"),
        "non-final output missing: {}",
        o.stdout
    );
    assert!(
        o.stdout.contains("x=result"),
        "final capture wrong: {}",
        o.stdout
    );
}

#[test]
fn higher_order_capture() {
    // Call-site mode instantiation: the higher-order function's Var output mode
    // is resolved to Bytes from the argument thunk's syntactic mode.
    let o = run("let f = { |cmd| !$cmd }\nlet x = f { printf hello }\necho \"[$x]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "[hello]");
}

#[test]
fn to_json_returns_bytes() {
    // to-json is a dual-channel encoder: it emits bytes on the output channel
    // AND returns those bytes as a Bytes value.  In let position, the Bytes
    // value is bound directly (not decoded to String).  Verify by roundtripping
    // through from-json — route the Bytes value via to-bytes (emit-stage).
    let o = run("let b = to-json [a: 1]\nlet obj = !{to-bytes $b | from-json}\necho $obj[a]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "1");
}

#[test]
fn to_bytes_non_utf8_succeeds() {
    // to-bytes returns Bytes, not String — so non-UTF-8 byte sequences
    // are not passed through String::from_utf8 and do not produce an error.
    let o = run("let b = to-bytes [255, 0, 254]\necho !{length $b}");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn to_json_via_user_wrapper() {
    // to-json is a primitive; users who want a first-class handle wrap it
    // in a block.  Roundtrip: encode → bytes → decode.
    let o = run(
        "let f = { |v| to-json $v }\nlet b = !{f [a: 42]}\nlet obj = !{to-bytes $b | from-json}\necho $obj[a]",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "42");
}

#[test]
fn block_mixed_modes_returns_value() {
    // When the last command of a block is value-returning, no capture buffer
    // is installed; the value is bound directly.  Preceding byte-output
    // commands' output goes to the terminal as a side-effect (visible in stdout
    // since the test harness captures ral's stdout).
    let o = run("let x = !{ echo hello; length [1, 2, 3] }\necho $x");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    // "hello" appears from the non-captured echo; "3" appears from echo $x.
    let last = o.stdout.trim().lines().last().unwrap_or("");
    assert_eq!(last, "3", "full stdout: {:?}", o.stdout);
}
