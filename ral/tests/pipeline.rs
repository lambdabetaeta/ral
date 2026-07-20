#![allow(clippy::disallowed_methods)]
// Integration tests for pipeline mechanics: process groups, signal routing,
// broken pipes, concurrent spawned pipelines, and output correctness.
//
// All tests run ral as a subprocess.  Signal relay and tcsetpgrp are
// only active in the interactive shell (is_interactive=true); these tests
// exercise the batch-mode plumbing — process group setup, pipe wiring,
// exit-status propagation — which is shared with the interactive shell.
//
// Unix-only: these tests rely on Unix commands (/bin/echo, grep, cat, yes,
// head, wc) and Unix process-group / signal semantics.  The portable
// subset — pure-value pipelines, capture semantics, stdin-consuming
// builtins — was extracted to pipeline_value_edges.rs, which runs on
// every platform.
#![cfg(unix)]

mod common;

use common::{Output, fresh_tmp_path, ral_bin};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

fn run(script: &str) -> Output {
    common::run("ral_test", script)
}

fn run_with_timeout(args: &[&str], script: &str, timeout: Duration) -> Option<Output> {
    common::run_with_timeout("ral_test", args, script, timeout)
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

#[cfg(feature = "ripgrep")]
#[test]
fn external_pipeline_bundled_rg() {
    let o = run("/bin/echo hello | rg hello");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hello");
}

#[cfg(feature = "ripgrep")]
#[test]
fn external_pipeline_bundled_rg_no_match_exits_one() {
    let o = run("/bin/echo hello | rg zzz");
    assert_ne!(o.status, 0);
    assert!(o.stdout.trim().is_empty());
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
    let script = r"/bin/sh -c 'head -c 131072 /dev/zero >&2'";
    let o = run_with_timeout(&["--audit"], script, Duration::from_secs(5))
        .expect("audited external command timed out — probable stdout/stderr pipe deadlock");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stderr.contains("\"stderr\""), "stderr: {}", o.stderr);
}

#[test]
fn audit_cli_captures_command_stdout() {
    // SPEC §10.3: every command node in the emitted execution tree
    // populates its `stdout` / `stderr` fields.  Pre-fix `ral --audit`
    // called `shell.local.audit.enable()` but left the byte-capture policy
    // at the default `CapturePolicy::Off`, so command nodes carried
    // empty buffers regardless of what the command printed.  The fix
    // mirrors the `audit { … }` builtin and sets the policy to
    // `CapturePolicy::Bytes`; this test pins that contract end-to-end
    // by running a script that prints a unique marker and asserting
    // the marker appears inside the audit-tree JSON's `stdout` field
    // (not merely anywhere in stderr — the marker also leaks into
    // `args` and is forwarded by /bin/echo to the inherited stdout,
    // so a loose substring check would pass even without the fix).
    let marker = "ral_audit_capture_marker_42";
    let script = format!("/bin/echo {marker}\n");
    let o = run_with_timeout(&["--audit"], &script, Duration::from_secs(5))
        .expect("audited echo timed out");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    // The audit dump is compact JSON; with `value_to_json_lossy_bytes`
    // the captured stdout bytes are rendered as a lossy-UTF-8 string
    // immediately following the `"stdout":"` key.  Anchoring the match
    // on `"stdout":"<marker>` rules out hits coming from `args` or
    // from the unrelated outer stdout passthrough.
    let needle = format!("\"stdout\":\"{marker}");
    assert!(
        o.stderr.contains(&needle),
        "expected substring {needle:?} in audit dump (the command \
         node's stdout field must carry the captured bytes per \
         SPEC §10.3); stderr: {}",
        o.stderr
    );
}

#[test]
fn redirect_stderr_to_stdout_flows_through_pipeline() {
    // Inner block captures stdout (with 2>&1 merging stderr in) as a String
    // via the byte-mode bind capture; from-string is then identity on String.
    let o =
        run("let s = !{!{/bin/sh -c 'printf out; printf err >&2' 2>&1} | from-string}\necho $s");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "outerr");
}

// ── Stage dispatch parity (handlers, ^name, redirects) ─────────────────────
//
// These regressions cover the rule that pipeline-stage dispatch must match
// `command_call::run_call`: a `within [handlers: …]` interception of an external
// name must fire even mid-pipeline, `^name` must skip binding lookup
// (pipeline included), and stage-level redirects must be honored rather than
// silently dropped.

#[test]
fn pipeline_stage_handler_intercepts_unknown_external() {
    // `mycmd` is not a builtin and (assumedly) not on PATH.  Without the
    // handler-match check in analyze_stage, the pipeline classifies the
    // stage as External and the launcher tries to spawn `mycmd`, failing
    // with ENOENT before the handler can run.
    let o = run(
        "within [handlers: [mycmd-pipeline-test: { |args| /bin/echo handled }]] \
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
    // command_call::run_call's rule: per-name handlers fire unconditionally;
    // ^name bypasses binding lookup but does NOT escape an explicit
    // per-name handler frame.  Pipeline-stage classification must agree
    // with the single-command path — otherwise `^cat X | cat` would
    // bypass the handler when the same call outside a pipeline would
    // honor it.  Locked in via the shared resolve_command_word.
    let o = run(
        "within [handlers: [cat: { |args| /bin/echo via-handler }]] \
            { ^cat IGNORED | cat }",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "via-handler");
}

#[test]
fn pipeline_external_stage_rejects_list_arg_with_hint() {
    // Passing a List as a positional arg to an external stage must error
    // with the same diagnostic command::run produces — `...$xs` hint.
    // The shared command::vet path is what enforces this — pipeline
    // stages and single-command exec both run vet before spawn, so a
    // list arg cannot reach `execve` as a (likely garbled) stringification.
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
        &[],
        "let s = !{cat | from-lines}; let xs = !{stream-to-list $s}; echo done; echo !{length $xs}",
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
        .map_or(0, |d| d.as_nanos());
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

// ── Redirects on handler-resolved heads ────────────────────────────────────
//
// A trailing fd redirect on a command whose head resolves to a handler frame
// — a runtime `alias`, a `within [handlers:]` entry, or the catch-all
// `within [handler:]` — must be installed for the handler body, exactly as it
// is for the builtin and external arms.  Regression for the dropped-redirect
// gap in `command_call::run_call`'s handler arm: the targets were evaluated
// (paths resolved) but never installed, so a forwarded command's output went
// to the inherited fd and the redirect file was never created.

#[test]
fn aliased_command_stdout_redirect_is_honored() {
    // `alias` installs a handler frame.  `myecho … > file` must send the
    // forwarded `/bin/echo`'s stdout to the file, not the terminal.
    let path = fresh_tmp_path("ral_alias_stdout", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run(&format!(
        "alias myecho {{ |a| /bin/echo ...$a }}\nmyecho stdout_marker > '{path_str}'\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        !o.stdout.contains("stdout_marker"),
        "forwarded output leaked to the terminal: {}",
        o.stdout
    );
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("stdout_marker"),
        "alias redirect file did not receive the forwarded stdout"
    );
}

#[test]
fn aliased_command_stderr_redirect_is_honored() {
    // `2> file` on an aliased head captures the forwarded command's stderr,
    // mirroring the stdout direction.
    let path = fresh_tmp_path("ral_alias_stderr", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run(&format!(
        "alias myerr {{ |a| /bin/sh -c 'echo stderr_marker >&2' }}\nmyerr 2> '{path_str}'\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        !o.stderr.contains("stderr_marker"),
        "forwarded stderr leaked to the terminal: {}",
        o.stderr
    );
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("stderr_marker"),
        "alias redirect file did not receive the forwarded stderr"
    );
}

#[test]
fn aliased_command_stdin_redirect_is_honored() {
    // `< file` into an aliased head feeds the file to the forwarded command's
    // stdin: `with_redirects` installs the stdin source for the handler body
    // via `install_stdin_redirect`, and the forwarded `/bin/cat` consumes it.
    let path = fresh_tmp_path("ral_alias_stdin", "txt");
    let path_str = path.display().to_string();
    std::fs::write(&path, "stdin_marker\n").unwrap();

    let o = run(&format!(
        "alias mycat {{ |a| /bin/cat }}\nmycat < '{path_str}'\n"
    ));
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        o.stdout.trim_end(),
        "stdin_marker",
        "forwarded command did not read the redirected stdin file"
    );
}

#[test]
fn pipeline_stage_handler_redirect_to_file_is_honored() {
    // A handler-resolved pipeline stage classifies as a Ral stage, so its
    // redirect rides in the stage comp and must be installed when the helper
    // re-evaluates that comp through `run_call`.  `foo > file | cat` routes
    // the handler's stdout to the file; the pipe sees EOF, so `cat` emits
    // nothing — matching the single-command path.
    let path = fresh_tmp_path("ral_pipe_handler", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run(&format!(
        "within [handlers: [foo: {{ |args| /bin/echo stage_marker }}]] {{ foo > '{path_str}' | cat }}\n/bin/echo done\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("stage_marker"),
        "pipeline-stage handler redirect file did not capture the stage's stdout"
    );
    assert!(
        !o.stdout.contains("stage_marker"),
        "stage output leaked into the pipe instead of the file: {}",
        o.stdout
    );
    assert!(o.stdout.contains("done"), "stdout: {}", o.stdout);
}

// ── Broken pipe ──────────────────────────────────────────────────────────────

#[test]
fn broken_pipe_large_producer_small_consumer() {
    // yes generates infinite output; head reads 100 lines and closes the pipe.
    // The pipeline must not hang.
    let o = run_with_timeout(
        &[],
        "yes MARKER | head -100 | wc -l",
        Duration::from_secs(5),
    )
    .expect("pipeline timed out — probable broken-pipe deadlock");
    assert_eq!(o.status, 0);
    assert_eq!(o.stdout.trim(), "100");
}

#[test]
fn broken_pipe_very_large_count() {
    let o = run_with_timeout(
        &[],
        "yes DATA | head -10000 | wc -l",
        Duration::from_secs(10),
    )
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
    let script = r"
let handles = !{ map { |i|
    let v = $[$i * $i]
    !{spawn { /bin/echo $v | cat }}
} [1, 2, 3, 4, 5, 6, 7, 8] }
!{ map { |h|
    let r = await $h
    echo !{to-bytes $r[stdout] | from-string}
} $handles }
echo done
";
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
    let script = r"
let h = !{spawn { /bin/echo 42 | cat }}
let r = await $h
echo !{to-bytes $r[stdout] | from-string}
";
    let o = run(script);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("42"));
}

// ── Mixed pipeline output ────────────────────────────────────────────────────

#[test]
fn mixed_pipeline_range_to_wc() {
    // range 1 21 produces [1..20] — 20 elements.  to-lines encodes the
    // list as newline-separated bytes; grep -c counts non-empty lines.
    let o = run("range 1 21 | to-lines | grep -c .");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let count: u32 = o.stdout.trim().parse().expect("grep -c output");
    assert_eq!(count, 20);
}

#[test]
fn mixed_pipeline_range_grep() {
    let o = run("range 1 100 | to-lines | grep 42");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("42"));
}

// ── Stress: many sequential pipelines ───────────────────────────────────────

#[test]
fn many_sequential_pipelines_no_leak() {
    // Run 50 external pipelines in sequence.  If file descriptors or process
    // groups leak, this will exhaust them and start failing.
    //
    // This pipeline takes the direct-external path: no value edge, no
    // redirects on the stages, no byte audit capture, and no foreground
    // terminal handoff.  It still allocates byte pipes and process-group
    // state each iteration, so the test catches fd/pgid leaks without
    // exercising helper-evaluated stages.
    let script = r"
let _go = { |n|
    if $[$n <= 0] {} else {
        /bin/echo $n | cat | grep . > /dev/null
        _go $[$n - 1]
    }
}
_go 50
echo done
";
    let o = run_with_timeout(&[], script, Duration::from_mins(1))
        .expect("sequential pipeline stress timed out");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("done"));
}

// ── Stopped pipeline children ────────────────────────────────────────────────

#[test]
fn normal_exit_137_is_not_reported_as_sigkill() {
    let o = run("/bin/sh -c 'exit 137'");
    assert_eq!(o.status, 137);
    assert!(
        o.stderr.contains("sh: exited with status 137"),
        "stderr: {}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("killed by signal 9"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn pipeline_first_external_failure_wins_over_later_helper_failure() {
    let o = run("/bin/sh -c 'exit 42' | from-json");
    assert_eq!(o.status, 42, "stderr: {}", o.stderr);
    assert!(
        o.stderr.contains("sh: exited with status 42"),
        "stderr: {}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("from-json: EOF"),
        "later helper failure won first-failure policy: {}",
        o.stderr
    );
}

#[test]
fn real_sigkill_is_not_reported_as_plain_exit_137() {
    let o = run("/bin/sh -c 'kill -KILL $$'");
    assert_eq!(o.status, 137);
    assert!(
        o.stderr.contains("sh: killed by signal 9 (SIGKILL)"),
        "stderr: {}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("sh: exited with status 137"),
        "stderr: {}",
        o.stderr
    );
}

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
        &[],
        "/bin/sh -c 'kill -STOP $$' | cat",
        Duration::from_secs(5),
    )
    .expect("pipeline hung after child stopped — wait_handling_stop did not fire");
    assert_eq!(o.status, 128 + libc::SIGSTOP, "stderr: {}", o.stderr);
    assert!(
        o.stderr.contains("stopped by signal")
            && o.stderr.contains("SIGSTOP")
            && !o.stderr.contains("exited with status 137"),
        "stderr: {}",
        o.stderr
    );
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
        &[],
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

    let pid = child.id().cast_signed();

    // Let the pipeline start before sending the signal.  Either
    // outcome — SIGINT relayed to the spawned children, or
    // `signal::check` aborting cleanly mid-launch — is correct;
    // the deadline below is what we care about.  100 ms is plenty
    // for the external-only `NoTerminal` path that this test
    // exercises (no anchor reexec, no helper protocol).
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
fn read_string_from_non_utf8_pipeline_fails() {
    // from-string is strict UTF-8: invalid bytes must produce an error,
    // not silently corrupt the data with replacement characters.
    let o = run("let s = !{/usr/bin/printf '\\377\\376A' | from-string}\necho !{length $s}");
    assert_ne!(o.status, 0, "expected failure on non-UTF-8 input");
    assert!(
        o.stderr.contains("from-string: input is not valid UTF-8"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn ext_command_non_utf8_gives_named_error() {
    // Invalid UTF-8 output from an external command is a runtime error.
    let o = run("let xv = /usr/bin/printf '\\377'");
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
fn fold_lines_from_pipeline() {
    // Count lines using fold-lines with an integer accumulator.
    let o = run(
        r#"let n = !{/bin/echo -e "a\nb\nc" | fold-lines { |acc _| return $[$acc + 1] } 0}
echo $n"#,
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

// ── Step decode + materialisation over byte pipelines ─────────────────────────

#[test]
fn internal_decode_to_step_then_list() {
    // ext → from-lines (internal Step decode) → stream-to-list materialisation.
    let o = run(r#"let s = !{/bin/echo -e "a
b
c" | from-lines}
let result = !{stream-to-list $s}
echo !{length $result}"#);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn from_lines_step_materialisation_matches_roundtrip() {
    // Materialising `from-lines` to a list should agree with a line count
    // computed via `fold-lines` on the same byte-producing command.
    //
    // Running inside the ral process's working directory (workspace root).
    let o = run(r#"
let s_direct = find . -name "*.rs" -not -path "./target/*" | from-lines
let direct = !{stream-to-list $s_direct}
let n = !{find . -name "*.rs" -not -path "./target/*" | fold-lines { |acc _| return $[$acc + 1] } 0}
echo !{length $direct}
echo $n
"#);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<&str> = o.stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected two count lines, got: {lines:?}");
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
        bwrap_functional()
    }
    #[cfg(target_os = "macos")]
    {
        macos_sandbox_functional()
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
    //
    // `--dev /dev` mirrors what the real `FsProjection::Restricted`
    // sandbox uses (see `core/src/sandbox/linux.rs`).  Mounting a fresh
    // devpts requires either CAP_SYS_ADMIN or a kernel that lets
    // unprivileged user namespaces do it — both absent under many
    // container runtimes.  Probing for it here keeps the IPC subprocess
    // tests skipped (instead of mass-failing with `bwrap: Can't mount
    // devpts: Permission denied`) in those environments.
    std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/lib",
            "/lib",
            "--dev",
            "/dev",
            "--",
            "/usr/bin/true",
        ])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
        .is_ok_and(|s| s.success())
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
        "let xv = grant [exec: ['/bin/echo': []], fs: [read: ['/tmp']]] { let s = !{/bin/echo captured | from-lines}; stream-to-list $s }; echo $xv[0]",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "captured");
}

#[test]
fn grant_pipeline_abort_after_missing_later_stage_does_not_hang() {
    if !sandbox_functional() {
        return;
    }
    for n in [5, 50_000] {
        let script = format!(
            "grant [net: false, fs: [read: ['cwd:', '/tmp', 'tempdir:'], write: ['cwd:', '/tmp', 'tempdir:']]] {{ range 0 {n} | limit 80 }}"
        );
        let o = run_with_timeout(&[], &script, Duration::from_secs(5))
            .expect("sandboxed value-to-missing-external pipeline hung");
        assert_eq!(o.status, 127, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
        assert!(
            o.stderr.contains("limit: command not found"),
            "missing-command diagnostic absent for n={n}: {}",
            o.stderr,
        );
        assert!(
            !o.stderr
                .contains("pipeline helper: parent closed before sending a stage job"),
            "abort path leaked helper diagnostic for n={n}: {}",
            o.stderr,
        );
    }
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
    let o = run(
        "let xv = /bin/echo piped | grant [fs: [read: ['/tmp']]] { let s = !{from-lines}; stream-to-list $s }; echo $xv[0]",
    );
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
    let o = run("let ee = []; echo hi | /usr/bin/printf '[%s]\\n' --flag '' ...$ee");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "[--flag]\n[]\n");
}

#[test]
fn pipeline_external_stage_expands_nonempty_spread() {
    let o = run("let ee = ['-n', 'hello']; echo hi | /usr/bin/printf '[%s]\\n' --flag ...$ee");
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
        .map_or(0, |d| d.as_nanos());
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

// ── Foreground-handoff race regressions ─────────────────────────────────────
//
// The pipeline anchor (a tiny null-stdio ral helper) keeps the pgid alive
// even when the first stage exits before later stages spawn.  These tests
// exercise that invariant by running ral itself as a pipeline stage with
// the hidden `--ral-test-pgid-check <tag>` flag, which writes its pgid to
// stderr and exits 0.  In a pre-anchor build, a fast producer like
// `printf ""` could let the pgid go away before the consumer joined it,
// stranding the consumer; the assertions below would either time out or
// observe inconsistent pgids.

fn parse_tagged_pgid(stderr: &str, tag: &str) -> Option<i32> {
    let prefix = format!("pgid:{tag}=");
    stderr
        .lines()
        .find_map(|line| {
            line.find(&prefix)
                .map(|start| &line[start + prefix.len()..])
        })
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
}

#[test]
fn race_short_producer_does_not_strand_consumer() {
    // `printf ""` exits immediately.  The consumer (a ral pgid probe) must
    // still join the pipeline pgid and run to completion.
    let ral = ral_bin();
    let script = format!("printf \"\" | {} --ral-test-pgid-check post", ral.display());
    let o = run_with_timeout(&[], &script, Duration::from_secs(5))
        .expect("pipeline hung after short producer — anchor lost?");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        parse_tagged_pgid(&o.stderr, "post").is_some(),
        "consumer did not run; stderr: {}",
        o.stderr
    );
}

#[test]
fn race_true_producer_does_not_strand_consumer() {
    // `/usr/bin/true` (the external, byte-typed) exits immediately with no
    // bytes.  Distinct from ral's value-typed `true` builtin, which would
    // be a value-to-byte mismatch — this test deliberately exercises the
    // byte-pipeline path.
    let ral = ral_bin();
    let script = format!(
        "/usr/bin/true | {} --ral-test-pgid-check post",
        ral.display()
    );
    let o = run_with_timeout(&[], &script, Duration::from_secs(5))
        .expect("pipeline hung after `true` producer");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        parse_tagged_pgid(&o.stderr, "post").is_some(),
        "consumer did not run; stderr: {}",
        o.stderr
    );
}

#[test]
fn pipeline_two_stages_share_anchor_pgid() {
    // Both stages must report the same pgid — the anchor's.
    let ral = ral_bin();
    let script = format!(
        "{} --ral-test-pgid-check up | {} --ral-test-pgid-check down",
        ral.display(),
        ral.display(),
    );
    let o = run_with_timeout(&[], &script, Duration::from_secs(5)).expect("pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let up = parse_tagged_pgid(&o.stderr, "up").expect("up pgid in stderr");
    let down = parse_tagged_pgid(&o.stderr, "down").expect("down pgid in stderr");
    assert_eq!(
        up, down,
        "stages did not share a pgid; stderr: {}",
        o.stderr
    );
}

#[test]
fn pipeline_three_stages_share_anchor_pgid() {
    let ral = ral_bin();
    let script = format!(
        "{r} --ral-test-pgid-check a | {r} --ral-test-pgid-check b | {r} --ral-test-pgid-check c",
        r = ral.display(),
    );
    let o = run_with_timeout(&[], &script, Duration::from_secs(5)).expect("pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let a = parse_tagged_pgid(&o.stderr, "a").expect("a pgid");
    let b = parse_tagged_pgid(&o.stderr, "b").expect("b pgid");
    let c = parse_tagged_pgid(&o.stderr, "c").expect("c pgid");
    assert_eq!(a, b, "stages a/b differ; stderr: {}", o.stderr);
    assert_eq!(b, c, "stages b/c differ; stderr: {}", o.stderr);
}

#[test]
fn pipeline_pgid_is_distinct_from_parent() {
    // The anchor establishes a fresh pgid for the pipeline; the consumer's
    // pgid must not be the parent ral's pgid.  Otherwise `tcsetpgrp` on
    // the pipeline group would steal the terminal from ral itself.
    let ral = ral_bin();
    let script = format!(
        "/usr/bin/true | {} --ral-test-pgid-check probe",
        ral.display()
    );
    let o = run_with_timeout(&[], &script, Duration::from_secs(5)).expect("pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let probe = parse_tagged_pgid(&o.stderr, "probe").expect("probe pgid");
    let parent = unsafe { libc::getpgrp() };
    assert_ne!(
        probe, parent as i32,
        "pipeline stage shared parent pgid; stderr: {}",
        o.stderr
    );
}

#[test]
fn pipeline_mid_stage_launch_failure_does_not_hang() {
    // Stage 2 references a command that cannot be resolved, so its
    // launch fails after stage 1 has already spawned.  The Drop chain
    // on RunningPipeline / RunningChild must SIGKILL the pgid and reap
    // every already-spawned child.  If any child leaks the wait()
    // inside the harness will time out.
    let script = "/usr/bin/true | /no/such/binary_xyzzy | /usr/bin/cat";
    let o = run_with_timeout(&[], script, Duration::from_secs(5))
        .expect("pipeline hung after mid-stage launch failure — child leak?");
    assert_ne!(o.status, 0, "stderr: {}", o.stderr);
}

#[test]
fn pipeline_mid_stage_launch_failure_with_long_producer_kills_it() {
    // Stage 1 is a long-running producer (`yes`); stage 2 fails to launch.
    // The producer must be killed (SIGKILL) on the abort path; otherwise
    // it would keep writing to its now-orphaned pipe forever and the test
    // would time out.  This is the canonical Drop-chain regression.
    let script = "/usr/bin/yes | /no/such/binary_xyzzy";
    let o = run_with_timeout(&[], script, Duration::from_secs(5))
        .expect("pipeline hung — long producer not killed on abort?");
    assert_ne!(o.status, 0, "stderr: {}", o.stderr);
}

#[test]
fn race_repeats_deterministically() {
    // Run the short-producer race a number of times in a row.  The
    // anchor + deferred-job protocol should make this deterministic; in a
    // pre-anchor build, occasional timeouts would surface here.
    let ral = ral_bin();
    let script = format!("printf \"\" | {} --ral-test-pgid-check post", ral.display());
    for i in 0..20 {
        let o = run_with_timeout(&[], &script, Duration::from_secs(5))
            .unwrap_or_else(|| panic!("iteration {i}: pipeline hung"));
        assert_eq!(o.status, 0, "iteration {i} stderr: {}", o.stderr);
        assert!(
            parse_tagged_pgid(&o.stderr, "post").is_some(),
            "iteration {i}: missing pgid; stderr: {}",
            o.stderr
        );
    }
}

// ── Foreground-handoff regressions ──────────────────────────────────────────
//
// A foreground pipeline that owns a tty cannot admit direct external
// launch: `resolve::direct_spawnable` forces such stages through the ral
// helper, and launch releases helper job frames only after `tcsetpgrp`
// has handed the pty to the pipeline pgid.  The tests below open a real
// pty so `tcgetpgrp` is meaningful.
//
// `--ral-test-pgid-check <tag>` writes both `pgid:<tag>` and (when
// stdin is a tty) `tcpgrp:<tag>` to stderr.  These tests ask: when the
// stage starts running user code, is it in the pgid that owns the
// controlling terminal?

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod pty_helper {
    use std::ffi::CStr;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};

    pub struct Pty {
        pub master: OwnedFd,
        pub slave_path: std::path::PathBuf,
    }

    /// Allocate a pty pair on Unix using `posix_openpt` / `grantpt` /
    /// `unlockpt` / `ptsname`.  The slave is only opened by the child
    /// (ral): the parent keeps the master to drive the test.  Caller
    /// is responsible for closing both.
    pub fn open() -> std::io::Result<Pty> {
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::grantpt(master) } != 0 || unsafe { libc::unlockpt(master) } != 0 {
            unsafe { libc::close(master) };
            return Err(std::io::Error::last_os_error());
        }
        let name_ptr = unsafe { libc::ptsname(master) };
        if name_ptr.is_null() {
            unsafe { libc::close(master) };
            return Err(std::io::Error::last_os_error());
        }
        let cstr = unsafe { CStr::from_ptr(name_ptr) };
        let slave_path = std::path::PathBuf::from(cstr.to_str().expect("ptsname utf-8").to_owned());
        let master = unsafe { OwnedFd::from_raw_fd(master as RawFd) };
        Ok(Pty { master, slave_path })
    }

    /// Open the slave side of a previously-allocated pty pair.
    pub fn open_slave(path: &std::path::Path) -> std::io::Result<OwnedFd> {
        use std::os::unix::ffi::OsStrExt;
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        let fd = unsafe { libc::open(bytes.as_ptr().cast(), libc::O_RDWR | libc::O_NOCTTY) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    /// Make `fd` the controlling terminal of the calling session.  Must
    /// be called from a fresh session (`setsid`) — typical post-fork
    /// pre-exec discipline for "I am the new session leader".
    ///
    /// `TIOCSCTTY` is typed differently across platforms (`Ioctl` on
    /// Linux, `c_uint` on Apple, `c_ulong` on the BSDs); the `as _`
    /// cast lets the request argument coerce to whatever `ioctl`
    /// expects on the target.
    pub unsafe fn become_controlling(fd: RawFd) -> std::io::Result<()> {
        if unsafe { libc::ioctl(fd, libc::TIOCSCTTY.into(), 0) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_pty_repl_until(
    line: &str,
    timeout: Duration,
    done: impl Fn(&str) -> bool,
) -> Option<Output> {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;

    let pty = pty_helper::open().ok()?;
    let slave_path = pty.slave_path.clone();

    let mut cmd = Command::new(ral_bin());
    cmd.arg("-i").arg("--norc");
    cmd.env("RAL_INTERACTIVE_MODE", "minimal");
    let slave_path_for_child = slave_path;
    unsafe {
        cmd.pre_exec(move || {
            // New session, then make the pty our controlling terminal
            // and dup it onto fds 0/1/2.  Errors propagate as `execve`-
            // time failures, which the parent sees via `wait`.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let slave = pty_helper::open_slave(&slave_path_for_child)?;
            let raw = slave.as_raw_fd();
            pty_helper::become_controlling(raw)?;
            for target in [0, 1, 2] {
                if libc::dup2(raw, target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;

    let dup = unsafe { libc::dup(pty.master.as_raw_fd()) };
    if dup < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut input = unsafe { std::fs::File::from_raw_fd(dup) };
    if writeln!(&mut input, "{line}").is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    drop(input);

    let raw = pty.master.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let reader_dup = unsafe { libc::dup(raw) };
    if reader_dup < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut reader = unsafe { std::fs::File::from_raw_fd(reader_dup) };
    let mut read_available = |bytes: &mut Vec<u8>| {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
    };

    let start = std::time::Instant::now();
    let mut bytes = Vec::new();
    let status = loop {
        read_available(&mut bytes);
        let text = String::from_utf8_lossy(&bytes);
        if done(&text) {
            let _ = child.kill();
            let _ = child.wait();
            break 0;
        }
        match child.try_wait().ok()? {
            Some(s) => break s.code().unwrap_or(1),
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break 124;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    // Drain the pty master with a short post-exit window: ral writes
    // its trace lines just before exiting, and the slave-side fds may
    // outlive the exit by a few millis on Linux.
    let drain_start = std::time::Instant::now();
    while drain_start.elapsed() < Duration::from_millis(200) {
        read_available(&mut bytes);
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(Output {
        stdout: String::new(),
        stderr: String::from_utf8_lossy(&bytes).into_owned(),
        status,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn parse_tagged_tcpgrp(stderr: &str, tag: &str) -> Option<i32> {
    let prefix = format!("tcpgrp:{tag}=");
    stderr
        .lines()
        .find_map(|line| {
            line.find(&prefix)
                .map(|start| &line[start + prefix.len()..])
        })
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn pty_external_stage_runs_to_completion() {
    // Sanity-check helper-gated foreground handoff under a real pty and
    // REPL: foreground handoff is an interactive-only policy, so this
    // drives ral through `-i --norc` rather than the batch script
    // runner.  The helper reports both its pgid and the pty foreground
    // pgid; after launch releases its job frame they must match.
    let ral = ral_bin();
    let script = format!("printf \"\" | {} --ral-test-pgid-check post", ral.display());
    let o = run_pty_repl_until(&script, Duration::from_secs(8), |text| {
        parse_tagged_pgid(text, "post").is_some()
            && parse_tagged_tcpgrp(text, "post").is_some()
            && text.matches("❯").count() >= 2
    })
    .expect("pty setup failed");
    assert_ne!(
        o.status, 124,
        "pty pipeline timed out; stderr: {}",
        o.stderr
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let pgid = parse_tagged_pgid(&o.stderr, "post")
        .unwrap_or_else(|| panic!("consumer did not run; stderr: {}", o.stderr));
    let tcpgrp = parse_tagged_tcpgrp(&o.stderr, "post")
        .unwrap_or_else(|| panic!("consumer did not report tcpgrp; stderr: {}", o.stderr));
    assert_eq!(tcpgrp, pgid, "stderr: {}", o.stderr);
}

// ── Process-staged ral helper protocol regressions ──────────────────────────

#[test]
fn ral_helper_returns_large_final_value_through_report() {
    // The final ral helper's value comes back inside the ChildEvalResponse
    // frame (drained concurrently by a parent reader thread).  A
    // pre-Fix-2 build read the value off a separate fd *after* waiting
    // on the child; if the value exceeded the kernel buffer, the
    // helper would block writing while the parent blocked waiting —
    // a circular wait.  This script forces a Bytes value far larger
    // than a typical pipe buffer (~64 KiB) through a process-staged
    // pipeline whose final stage is a ral helper (`from-bytes`).
    //
    // `length` of a 200 KiB Bytes value confirms we got the whole
    // value out without truncation or deadlock.
    let script = r"
let bs = !{ /usr/bin/yes | head -c 200000 | from-bytes }
echo !{length $bs}
";
    let o = run_with_timeout(&[], script, Duration::from_secs(15))
        .expect("large-value pipeline hung — report-channel deadlock?");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "200000");
}

#[test]
fn ral_helper_emits_large_audit_payload_without_deadlock() {
    // A ral helper stage runs a nested external that writes 200 KiB
    // of stderr.  Under `--audit` the nested external's stderr is
    // captured by the helper's audit tree and rides back to the
    // parent inside the ChildEvalResponse frame.  Without a concurrent
    // reader thread the helper would block writing the report (full
    // socket buffer) while the parent blocked waiting on the helper
    // — a circular wait this test would catch as a 15-second hang.
    let script = r#"
let s = !{ /bin/echo "" | from-string | { |x| /bin/sh -c 'head -c 200000 /dev/zero >&2'; return $x } }
echo done
"#;
    let o = run_with_timeout(&["--audit"], script, Duration::from_secs(15))
        .expect("audited pipeline hung — report drain blocked?");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("done"), "stdout: {}", o.stdout);
}

#[test]
fn pipeline_path_literal_exec_failure_reports_127() {
    // `/no/such/binary` cannot be spawned.  Direct external launch
    // rebuilds that as `CommandFailure::Spawn(NotFound)` against the
    // real command name and surfaces a "no such file or directory"
    // diagnostic, not a generic pipeline-stage failure.
    let o = run_with_timeout(
        &[],
        "/no/such/binary | /usr/bin/cat",
        Duration::from_secs(5),
    )
    .expect("path-literal exec failure pipeline hung");
    assert_eq!(o.status, 127, "stderr: {}", o.stderr);
    assert!(
        o.stderr.contains("/no/such/binary"),
        "diagnostic must name the user's command, not the trampoline; stderr: {}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("helper exited"),
        "must not surface the generic helper-exit diagnostic; stderr: {}",
        o.stderr
    );
}

#[test]
fn pipeline_permission_denied_path_reports_126() {
    // A non-executable file in a pipeline produces ExecReport with
    // PermissionDenied and exit 126 — the POSIX status for "found
    // but not executable".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not_exec");
    std::fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
    // No execute bit.
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&path, perms).unwrap();

    let script = format!("{} | /usr/bin/cat", path.display());
    let o = run_with_timeout(&[], &script, Duration::from_secs(5))
        .expect("permission-denied pipeline hung");
    assert_eq!(o.status, 126, "stderr: {}", o.stderr);
    assert!(
        o.stderr.to_lowercase().contains("permission denied"),
        "must mention permission denied; stderr: {}",
        o.stderr
    );
}

// The non-transferable retained-value invariant — that a byte-mode
// helper does not pay for serialising its (unused) return value — is
// covered by `runtime::pipeline::helper::tests::stage_job_skips_report_value_when_parent_does_not_need_it`
// at the protocol layer.  Constructing an end-to-end script that
// exercises it is awkward because byte-mode helper bodies naturally
// return Unit; the unit test names the contract more clearly than any
// indirect integration shape.

// ── Audit + redirect: no panic ───────────────────────────────────────────────
//
// A stage with `> file` (or `2> file`, or `> file 2>&1`) under
// `--audit` legitimately has no parent-side pump to tee from — the
// kernel routed the bytes straight into the file.  Pre-fix
// `make_audit_capture` panicked here ("audit mode allocates per-stage
// stdout buffer"); now it stores `None` and the join path records
// empty captured bytes alongside the redirected file.

#[test]
fn audited_stdout_redirect_does_not_panic() {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("ral_audit_redir_stdout_{pid}_{nanos}.txt"));
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let script = format!("/bin/echo redirected > '{path_str}' | cat\n/bin/echo done\n");
    let o = run_with_timeout(&["--audit"], &script, Duration::from_secs(5))
        .expect("audited stdout-redirect pipeline hung");
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("redirected"),
        "redirect target did not receive bytes; stderr: {}",
        o.stderr
    );
}

#[test]
fn audited_stderr_redirect_does_not_panic() {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("ral_audit_redir_stderr_{pid}_{nanos}.txt"));
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let script = format!("/bin/sh -c 'echo to-stderr >&2' 2> '{path_str}' | cat\n/bin/echo done\n");
    let o = run_with_timeout(&["--audit"], &script, Duration::from_secs(5))
        .expect("audited stderr-redirect pipeline hung");
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("to-stderr"),
        "stderr redirect target did not receive bytes"
    );
}

#[test]
fn audited_stdout_and_stderr_redirect_does_not_panic() {
    // `> file 2>&1` joins both streams into the same file.  Pre-fix
    // the audited path panicked because both `stdout_buf` and
    // `stderr_buf` were `None`.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = std::env::temp_dir().join(format!("ral_audit_redir_both_{pid}_{nanos}.txt"));
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let script =
        format!("/bin/sh -c 'echo o; echo e >&2' > '{path_str}' 2>&1 | cat\n/bin/echo done\n");
    let o = run_with_timeout(&["--audit"], &script, Duration::from_secs(5))
        .expect("audited stdout+stderr-redirect pipeline hung");
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let body = body.unwrap_or_default();
    assert!(body.contains('o') && body.contains('e'), "body: {body}");
}

// ── Audit survives helper errors ─────────────────────────────────────────────

#[test]
fn audited_failing_helper_preserves_nested_external_audit() {
    // A ral helper that runs a nested external and then fails must
    // still leave the nested external's audit node in the parent
    // tree.  Pre-fix `unpack_stage_report` discarded audit nodes on
    // structured-error reports.
    //
    // Shape: a process-staged pipeline (`printf hi` makes it
    // process-staged) whose final stage is a forced ral helper block. The
    // block runs `/bin/echo nested-record` (audit-captured by the
    // helper), reads the upstream bytes via `from-string`, and then
    // calls `fail` to report a structured failure.  The parent must
    // extend its audit tree with the nested external before
    // surfacing the helper's error.
    let script = r#"
printf hi | !{ let _s = !{from-string}; let _x = !{/bin/echo nested-record}; fail [status: 1, message: "helper failed"] }
"#;
    let o = run_with_timeout(&["--audit"], script, Duration::from_secs(5))
        .expect("audited failing-helper pipeline hung");
    assert_ne!(o.status, 0, "expected helper failure to bubble up");
    assert!(
        o.stderr.contains("nested-record"),
        "audit must record the nested external even when the helper fails; stderr: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("helper failed"),
        "structured helper error must surface; stderr: {}",
        o.stderr
    );
}
