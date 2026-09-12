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
// subset — capture semantics and stdin-consuming builtins — was extracted to
// pipeline_value_edges.rs, which runs on every platform.
#![cfg(unix)]

mod common;

use common::{Output, fresh_tmp_path, ral_bin, ral_command};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
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
    // SPEC §13.3: every command observation in the emitted audit trail
    // populates its `stdout` / `stderr` fields, so `ral --audit` must
    // install `CapturePolicy::Bytes` as the `audit { … }` builtin does;
    // the default `Off` leaves every observation's buffers empty.  The
    // marker has to land inside the audit dump's `stdout` field, not
    // merely anywhere in stderr — it also leaks into `args` and is
    // forwarded by /bin/echo to the inherited stdout, so a loose
    // substring check would pass with capture off.
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
         observation's stdout field must carry the captured bytes per \
         SPEC §13.3); stderr: {}",
        o.stderr
    );
}

#[test]
fn redirect_stderr_to_stdout_flows_through_pipeline() {
    // Inner block captures stdout (with 2>&1 merging stderr in) as a String
    // via the capture a byte-routed bind inserts; from-string is then
    // identity on String.
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
    // handler-match check in `resolve_launch`, the pipeline classifies the
    // stage as external and the launcher tries to spawn `mycmd`, failing
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
fn pipeline_stage_caret_escapes_per_name_handler() {
    // ^name is the path-head arm too (ruling 2): it escapes a per-name
    // handler frame exactly as a bare path head does.  Pipeline-stage
    // classification must agree with the single-command path — the bundled
    // `cat` is what `echo hi | ^cat` reaches, cross-platform, while a bare
    // `cat` in the same block still honors the arm.  Locked in via the
    // shared resolve_command_word.
    let o = run(
        "within [handlers: [cat: { |args| /bin/echo via-handler }]] \
            { echo hi | ^cat }",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");

    let o = run(
        "within [handlers: [cat: { |args| /bin/echo via-handler }]] \
            { echo hi | cat }",
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
    //
    // The list arrives decoded rather than written, so that this is the vet
    // path and not the checker's: `from-json` returns a value of no particular
    // type, and only the run knows it is a list.
    let o = run("let xs = /bin/echo '[1, 2, 3]' | from-json; /bin/echo hi | /usr/bin/printf $xs");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("cannot pass List"),
        "stderr: {}",
        o.stderr
    );
    assert!(o.stderr.contains("...$"), "hint missing: {}", o.stderr);
}

#[test]
fn pipeline_external_stage_list_arg_written_out_is_a_static_error() {
    // Written out, the shape is in the type, and the same refusal comes from
    // the checker instead: nothing is spawned, and the stage never runs.
    let o = run("let xs = [1, 2, 3]; /bin/echo hi | /usr/bin/printf $xs");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("T0057")
            && o.stderr
                .contains("cannot pass [Integer] to external command '/usr/bin/printf'"),
        "stderr: {}",
        o.stderr
    );
    assert!(o.stderr.contains("...$"), "hint missing: {}", o.stderr);
    assert!(
        !o.stdout.contains("hi"),
        "a diagnosed pipeline must not run: {}",
        o.stdout
    );
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
fn deep_stream_returns_from_a_final_stage() {
    // A stream is one closure per line, returned from the final stage's
    // thread.  Two thousand lines must cross and then drop without
    // exhausting either thread's stack.  (Regression: the encoder recursed
    // per link, so the stage died once `from-lines` saw a few hundred lines.)
    let o = run("let s = !{seq 1 2000 | from-lines}; echo done");
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

/// A stage whose stdout never reaches the edge never writes a dead one, so a
/// stdin-ignoring reader settling first cannot cut it (SPEC §7.6).  The
/// file must be complete: a cut landing mid-rename on the stage's own atomic
/// write would report success over a file that was never created.
#[test]
fn a_redirect_stage_survives_a_reader_that_never_looks_at_stdin() {
    let path = fresh_tmp_path("ral_pipe_redir_fast_reader", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run(&format!(
        "/bin/echo redirected > '{path_str}' | /usr/bin/true\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("redirected"),
        "file did not receive redirected bytes — the stage was killed before its own \
         redirect committed"
    );
}

/// The same corollary through a `Scope(Redirect)` frame — the carrier a
/// closure call wraps its trailing redirect in, since it cannot fuse the
/// redirect onto itself the way an `Exec` node does.
#[test]
fn a_redirected_closure_stage_survives_a_reader_that_never_looks_at_stdin() {
    let path = fresh_tmp_path("ral_pipe_redir_closure", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run(&format!(
        "let f = {{ |x| /bin/echo $x }}\n$f hello > '{path_str}' | /usr/bin/true\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("hello"),
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

// ── Reader-gone forgiveness ──────────────────────────────────────────────────
//
// An interior edge is dead once its reader stage has ended; a stage is cut
// at its first write to a dead edge, and nowhere else — SPEC §7.6.  ral's
// own write raises the break at the write; an external's write is heard by
// ral (it reads the dead edge itself) and answered with SIGKILL, which is
// forgiven.  The tests below exercise that boundary: a producer's signal
// disposition cannot change the verdict, a producer that exits on its own
// account keeps its own status, everything a producer does before its first
// dead write is certain, forgiveness is scoped per edge through a middle
// stage, and a producer that never writes again is never cut at all.

#[test]
fn broken_pipe_very_large_count() {
    // `yes` generates infinite output; `head` reads its fill and closes the
    // pipe.  The pipeline must not hang.
    let o = run_with_timeout(
        &[],
        "yes DATA | head -10000 | wc -l",
        Duration::from_secs(10),
    )
    .expect("pipeline timed out");
    assert_eq!(o.status, 0);
    assert_eq!(o.stdout.trim(), "10000");
}

#[test]
fn a_firehose_into_a_non_reading_consumer_terminates() {
    // Neither side of a `|` promises traffic, and the producer's side of
    // that symmetry is the one with teeth: `yes` never stops writing, and
    // `!{ return 5 }` returns without ever touching stdin.  The edge dies
    // the instant the consumer ends; `yes`'s next write into it — whether
    // that write is still filling the pipe or blocked on a full one — is
    // heard by ral and answered with SIGKILL, forgiven.  The pipeline's
    // value is the consumer's own `5`; whatever `yes` wrote into the dead
    // edge was never read and is not in it.
    let o = run_with_timeout(
        &[],
        "let n = !{ /usr/bin/yes | !{ return 5 } }\necho $n\n",
        Duration::from_secs(10),
    )
    .expect("firehose hung — the consumer's read end outlived the consumer");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "5", "stdout: {}", o.stdout);
}

#[test]
fn sigpipe_ignoring_producer_is_still_forgiven() {
    // A producer that traps SIGPIPE has no broken-pipe signal to take in
    // the first place: it is cut at its first write past `head`'s exit —
    // ral reads that dead edge itself and answers with SIGKILL, forgiven —
    // the same verdict as an ordinary producer, whatever its signal
    // disposition, because the cut is never keyed on it.
    let o = run_with_timeout(
        &[],
        r#"sh -c 'trap "" PIPE; while :; do echo x; done' | head -1"#,
        Duration::from_secs(10),
    )
    .expect("sigpipe-ignoring producer hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "x", "stdout: {}", o.stdout);
}

#[test]
fn producer_own_exit_status_survives_early_reader_exit() {
    // A stage is cut only by a write to a dead edge.  `sh` never writes at
    // all — it just exits — so it is never cut, and its own exit status is
    // the pipeline's, exactly as an ordinary command's would be.
    let o = run_with_timeout(&[], "sh -c 'exit 7' | head -1", Duration::from_secs(5))
        .expect("producer-exit pipeline hung");
    assert_eq!(o.status, 7, "stderr: {}", o.stderr);
}

#[test]
fn escape_inside_a_killed_producer_never_fires() {
    // `yes`'s first write past `head`'s exit is a write to a dead edge, and
    // the cut lands there — mid-`yes`, before the block's `exit 5`
    // statement is ever reached: the escape never occurs, and the pipeline's
    // status is decided by `head`'s own (successful) exit, not by a
    // statement the killed stage never ran.
    let o = run_with_timeout(&[], "!{ yes ; exit 5 } | head -1", Duration::from_secs(10))
        .expect("escape-flip pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
}

#[test]
fn a_producers_own_account_work_runs_after_its_reader_left() {
    // `a` is delivered to a living `head -1`, which then exits; the
    // producer never writes to the edge again, so it is never cut and its
    // own account work — the marker file, the exit — runs to completion.
    let path = fresh_tmp_path("ral_pipe_own_account_marker", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let script = format!("sh -c 'echo a; sleep 0.3; : > {path_str}; exit 4' | head -1");
    let o =
        run_with_timeout(&[], &script, Duration::from_secs(5)).expect("own-account pipeline hung");
    let marker_exists = path.exists();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 4, "stderr: {}", o.stderr);
    assert!(
        marker_exists,
        "producer's own-account work after its reader left did not run"
    );
}

#[test]
fn a_dead_write_cuts_the_producer_there() {
    // `a` reaches a living `head -1`; `head` then exits, and `b` is the
    // producer's first write to the now-dead edge — the cut lands there,
    // during the following sleep, before the marker file is ever created.
    let path = fresh_tmp_path("ral_pipe_dead_write_marker", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let script =
        format!("sh -c 'echo a; sleep 0.3; echo b; sleep 0.3; : > {path_str}; exit 4' | head -1");
    let o =
        run_with_timeout(&[], &script, Duration::from_secs(5)).expect("dead-write pipeline hung");
    let marker_exists = path.exists();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        !marker_exists,
        "producer ran past its first dead write instead of being cut there"
    );
}

#[test]
fn effects_before_a_producers_first_dead_write_are_certain() {
    // A dead-edge cut can only ever land at a write to the edge itself: a
    // stderr write earlier in the same sequential stage completes before any
    // such write is attempted, so it is certain regardless of the race with
    // the reader's exit — for a direct external stage and for a ral block
    // wrapping one alike.
    for script in [
        "sh -c 'echo world >&2; echo hello' | true",
        "!{ sh -c 'echo world >&2; echo hello' } | true",
    ] {
        let o = run_with_timeout(&[], script, Duration::from_secs(5))
            .unwrap_or_else(|| panic!("{script}: pipeline hung"));
        assert_eq!(o.status, 0, "{script}: stderr: {}", o.stderr);
        assert!(o.stderr.contains("world"), "{script}: stderr: {}", o.stderr);
    }
}

#[test]
fn a_ral_stages_file_effect_before_its_first_write_is_certain() {
    // `echo x > FILE` is a per-command redirect: its bytes never touch the
    // edge, so the file write is certain regardless of the race between the
    // block's later `echo hello` (a dead write, since the reader never
    // touches stdin) and the reader's own exit.
    let path = fresh_tmp_path("ral_pipe_stage_redirect_marker", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let script = format!("!{{ echo x > '{path_str}'; echo hello }} | !{{ return () }}");
    let o = run_with_timeout(&[], &script, Duration::from_secs(5))
        .expect("ral-stage redirect pipeline hung");
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(body.as_deref().map(str::trim_end), Some("x"));
}

#[test]
fn middle_stage_forgiveness_is_per_edge() {
    // Forgiveness is scoped per interior edge: `cat`'s edge to `yes` and its
    // edge to `head` die independently once `head` exits, so `cat`'s next
    // write into `head`'s dead edge, and then `yes`'s next write into
    // `cat`'s now-dead edge, are each cut and forgiven in turn.
    let o = run_with_timeout(&[], "yes | cat | head -1", Duration::from_secs(10))
        .expect("middle-stage forgiveness pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
}

#[test]
fn a_nested_pipelines_producer_is_forgiven_when_the_outer_reader_leaves() {
    // The outer reader-gone cut reaches the nested pipeline as a `ReaderGone`
    // cancel, and a cut is a kill: opening on the producer's own SIGTERM
    // disposition would hand `yes` a death of its own to report, when what
    // ended it is this collector's doing and is forgiven.
    let o = run_with_timeout(&[], "!{ yes | cat } | head -1", Duration::from_secs(10))
        .expect("nested-pipeline forgiveness pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "y\n", "stdout: {}", o.stdout);
}

#[test]
fn a_producer_that_never_writes_is_never_cut_and_keeps_its_status() {
    // A stage is cut at its first write to a dead edge, and nowhere else:
    // a producer that never writes again — however long it runs — is never
    // cut, so its own exit status is the pipeline's, exactly as if the
    // reader had never left.
    let o = run_with_timeout(
        &[],
        "sh -c 'sleep 0.3; echo x >&2; exit 4' | !{ return 5 }",
        Duration::from_secs(5),
    )
    .expect("never-writing producer hung");
    assert_eq!(o.status, 4, "stderr: {}", o.stderr);
    assert!(o.stderr.contains('x'), "stderr: {}", o.stderr);
}

#[test]
fn a_never_writing_producers_failure_survives_a_fast_reader() {
    // A reader that exits at once, without ever looking at the edge, still
    // never turns the producer's own failure into forgiveness: the producer
    // never writes, so it is never cut, and its exit status wins.
    let o = run_with_timeout(&[], "sh -c 'exit 3' | true", Duration::from_secs(5))
        .expect("never-writing producer hung");
    assert_eq!(o.status, 3, "stderr: {}", o.stderr);

    let o = run_with_timeout(
        &[],
        "sh -c 'exit 3' | !{ return () }",
        Duration::from_secs(5),
    )
    .expect("never-writing producer hung");
    assert_eq!(o.status, 3, "stderr: {}", o.stderr);
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
    let res = await $h
    echo !{to-bytes $res[stdout] | from-string}
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

// ── Mixed pipeline output ────────────────────────────────────────────────────

#[test]
fn mixed_pipeline_range_to_wc() {
    // range 1 21 produces [1..20].  Apply the encoder explicitly, then let
    // grep count the newline-separated bytes.
    let o = run("to-lines !{range 1 21} | grep -c .");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let count: u32 = o.stdout.trim().parse().expect("grep -c output");
    assert_eq!(count, 20);
}

// ── Stress: many sequential pipelines ───────────────────────────────────────

#[test]
fn many_sequential_pipelines_no_leak() {
    // Run 50 pipelines in sequence, each with a ral-written stage thread in
    // the middle.  If file descriptors, process groups, or stage threads
    // leak, this will exhaust them and start failing.
    //
    // On Linux, also probe the process's own thread count before and after
    // (`/proc/$PPID/status` read by a nested external, since that external's
    // parent is ral itself) and assert it returns to baseline — every stage
    // thread this loop spawns must actually join.
    // The probe reads `/proc`, which only Linux has; elsewhere the stress
    // loop runs on its own.
    let (open, close) = if cfg!(target_os = "linux") {
        (
            r"
let probe = { /bin/sh -c 'grep Threads: /proc/$PPID/status' | !{from-line} }
let before = !{probe}
echo threads:before=$before
",
            r"
let after = !{probe}
echo threads:after=$after
",
        )
    } else {
        ("", "")
    };
    let script = format!(
        r"{open}
let _go = {{ |n|
    if $[$n <= 0] {{}} else {{
        /bin/echo $n | !{{filter-lines {{ |_| true }}}} | grep . > /dev/null
        _go $[$n - 1]
    }}
}}
_go 50
{close}
echo done
"
    );
    let o = run_with_timeout(&[], &script, Duration::from_mins(1))
        .expect("sequential pipeline stress timed out");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(o.stdout.contains("done"));

    if cfg!(target_os = "linux") {
        let before = parse_tagged_field(&o.stdout, "threads:before=")
            .expect("baseline thread count missing");
        let after =
            parse_tagged_field(&o.stdout, "threads:after=").expect("final thread count missing");
        assert_eq!(
            before, after,
            "stage-thread count did not return to baseline; stdout: {}",
            o.stdout
        );
    }
}

/// Pull `<prefix><rest of the line>` out of some text — the little-sibling
/// of `parse_tagged_pgid`, for tags this file only ever prints once.
fn parse_tagged_field(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::to_string)
}

// ── Exit-status classification and group edge cases ──────────────────────────

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
fn a_nested_pipeline_joins_its_stages_group() {
    // A pipeline launched from inside a ral-written stage thread must join
    // the outer pipeline's group rather than prepare its own: every stage
    // of `inner1 | inner2`, run from within the outer's first stage, and
    // the outer's own direct final stage, all share one anchor pgid.  The
    // final stage drains its stdin first, so the inner stages have already
    // reported and exited before their edge could ever die under them.
    let ral = ral_bin();
    let script = format!(
        "!{{ {r} --ral-test-pgid-check inner1 | {r} --ral-test-pgid-check inner2 }} \
         | sh -c 'cat >/dev/null; exec {r} --ral-test-pgid-check outer'",
        r = ral.display(),
    );
    let o = run_with_timeout(&[], &script, Duration::from_secs(5)).expect("nested pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let outer = parse_tagged_pgid(&o.stderr, "outer").expect("outer pgid");
    let inner1 = parse_tagged_pgid(&o.stderr, "inner1").expect("inner1 pgid");
    let inner2 = parse_tagged_pgid(&o.stderr, "inner2").expect("inner2 pgid");
    assert_eq!(
        outer, inner1,
        "inner1 did not join the outer group; stderr: {}",
        o.stderr
    );
    assert_eq!(
        outer, inner2,
        "inner2 did not join the outer group; stderr: {}",
        o.stderr
    );
}

#[test]
fn a_stages_error_keeps_its_span() {
    // A ral-written stage's error must point into the source line exactly
    // as the same failure would at top level — a stage thread evaluates
    // the same `Comp`, with no re-exec to lose the span across.
    let top_level = run("/bin/echo notjson | from-json");
    assert_ne!(top_level.status, 0, "stderr: {}", top_level.stderr);
    assert!(
        top_level.stderr.contains("from-json"),
        "top-level stderr: {}",
        top_level.stderr
    );

    let staged = run("/bin/echo notjson | !{ from-json }");
    assert_ne!(staged.status, 0, "stderr: {}", staged.stderr);
    assert!(
        staged.stderr.contains("from-json"),
        "staged stderr: {}",
        staged.stderr
    );
    // Both renderings underline the `from-json` token itself, not the
    // whole block or an empty span at 0:0.
    assert!(
        staged.stderr.contains("────┬────") || staged.stderr.contains("──┬──"),
        "staged error must carry a real span into the source line, not a synthetic one; stderr: {}",
        staged.stderr
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

/// `sleep 4 &` is forked first and holds the pumped stderr open under
/// `--audit`; `a` is a live write, delivered to `head` before it exits; `b`
/// is the dead write that cuts `sh` there.  The stage's own reader-gone kill
/// addresses its pid alone, so the backgrounded descendant survives it,
/// still holding the stage's pumped stderr open — the pump must be
/// detached, not joined, or the pipeline waits for the orphan.  Two seconds
/// discriminates: the descendant sleeps four.
#[test]
fn a_pumped_descendant_does_not_outlive_the_reader_gone_kill() {
    for args in [&[][..], &["--audit"][..]] {
        let o = run_with_timeout(
            args,
            "/bin/sh -c 'sleep 4 & echo a; sleep 0.5; echo b; wait' | head -1",
            Duration::from_secs(2),
        )
        .unwrap_or_else(|| panic!("{args:?}: the pipeline waited for the stage's descendant"));
        assert_eq!(o.status, 0, "{args:?}: stderr: {}", o.stderr);
    }
}

// ── SIGINT kills external child ──────────────────────────────────────────────

/// Run `script` in a ral of its own process group — so `kill(-pid)` reaches
/// exactly ral, not the test runner — `SIGINT` that group once `settle` has
/// let the pipeline start, and return how ral ended within 3 s: `None` if it
/// did not.  In batch mode the shell itself receives the SIGINT and cancels
/// the run, so it must *exit* — a status with no code means a signal killed
/// it (a `SIGPIPE` from its own interior edge, say).
fn ral_exits_after_sigint(script: &str, settle: Duration) -> Option<std::process::ExitStatus> {
    let tmp = fresh_tmp_path("ral_sigint", "ral");
    std::fs::write(&tmp, script).unwrap();
    let mut cmd = ral_command();
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
    std::thread::sleep(settle);
    unsafe {
        libc::kill(-child.id().cast_signed(), libc::SIGINT);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let ended = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    std::fs::remove_file(&tmp).ok();
    ended
}

fn assert_ral_exited_after_sigint(script: &str, settle: Duration, hang: &str) {
    let status = ral_exits_after_sigint(script, settle).unwrap_or_else(|| panic!("{hang}"));
    assert!(
        status.code().is_some(),
        "ral was killed by a signal instead of exiting: {status}"
    );
}

#[test]
fn sigint_kills_external_child_in_pipeline() {
    // The externals have SIG_DFL and die of the relayed signal.
    assert_ral_exited_after_sigint(
        "/bin/echo start | sleep 60\n",
        Duration::from_millis(100),
        "ral did not exit after SIGINT",
    );
}

#[test]
fn sigint_kills_a_ral_stage_blocked_reading_a_live_upstream() {
    // A stage thread blocked in `from-line` on a producer that never
    // writes: only the wake can end its read.
    assert_ral_exited_after_sigint(
        "sleep 100 | !{ from-line }\n",
        Duration::from_millis(200),
        "a stage thread blocked on a live upstream was not woken",
    );
}

#[test]
fn sigint_kills_a_ral_stage_blocked_writing_to_a_full_edge() {
    // The producer fills the edge (well under a second) and blocks in
    // `echo`.  Only the wake may end that write: the parent holds the edge's
    // read end, and an `EPIPE` there would be a `SIGPIPE` to the whole shell.
    assert_ral_exited_after_sigint(
        "!{ let go = { |n| echo tick; go $[$n + 1] }; go 0 } | sleep 100\n",
        Duration::from_secs(1),
        "a stage thread blocked writing into a full edge was not woken",
    );
}

#[test]
fn a_killed_producer_blocked_in_a_full_edge_does_not_take_the_shell() {
    // The reader drains past the pipe's capacity and exits with the
    // producer (a ral stage) blocked mid-write.  ral's own read of the
    // now-dead edge frees that blocked write; the stage's sink then sees
    // the dead edge and raises there — never an EPIPE surfacing under a
    // thread of this process.
    let o = run_with_timeout(
        &[],
        "!{ let go = { |n| echo tick; go $[$n + 1] }; go 0 } | sh -c 'head -c 200000 >/dev/null'",
        Duration::from_secs(10),
    )
    .expect("killed-producer pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
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
        o.stderr.contains("captured output is not valid UTF-8"),
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
    // Entering Seatbelt is what may be unavailable, and a failed entry aborts
    // the confined child — so run the smallest real grant through the public
    // surface and require both a clean exit and the body's own output.
    ral_command()
        .args([
            "--norc",
            "-c",
            "grant [exec: ['/bin/echo': 'allow'], fs: [read: ['/tmp']]] { /bin/echo probe }",
        ])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "probe")
}

#[test]
fn grant_fs_pipeline_stdout_flows() {
    // Grant is a pipeline stage: its stdout goes to a Pipe sink.
    // configure_subprocess_stdio must clone the pipe writer and hand it to
    // the IPC subprocess; cat on the right side must receive the output.
    if !sandbox_functional() {
        return;
    }
    let o = run(
        "grant [exec: ['/bin/echo': 'allow'], fs: [read: ['/tmp']]] { /bin/echo sandboxed } | cat",
    );
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
        "let xv = grant [exec: ['/bin/echo': 'allow'], fs: [read: ['/tmp']]] { let s = !{/bin/echo captured | from-lines}; stream-to-list $s }; echo $xv[0]",
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
            "grant [net: false, fs: [read: ['cwd:', '/tmp', 'tempdir:'], write: ['cwd:', '/tmp', 'tempdir:']]] {{ to-lines !{{range 0 {n}}} | limit 80 }}"
        );
        let o = run_with_timeout(&[], &script, Duration::from_secs(5))
            .expect("sandboxed byte-to-missing-external pipeline hung");
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

/// The regression this closes: `launch_external_stage_direct` spawns a
/// confined pipeline stage under `PgidPolicy::Join`, and until the collector
/// also addresses that stage's own envelope group, the group-wide grace
/// signal never reaches it — bwrap's monitor joins the pipeline group, the
/// payload does not.  The fixture's `TERM` trap writing its own evidence
/// file is the only honest witness: a trap that ends in `exit 0` exits
/// cleanly, so the outcome carries no sign that a signal ever arrived.
#[test]
fn a_cancelled_confined_pipeline_stage_gets_its_grace_signal() {
    if !sandbox_functional() {
        return;
    }
    let gate = fresh_tmp_path("ral_pipeline_confined_grace", "gate");
    let trapfile = fresh_tmp_path("ral_pipeline_confined_grace", "trap");
    let fixture = fresh_tmp_path("ral_pipeline_confined_grace", "sh");
    mkfifo(&gate);
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\ntrap 'echo GRACE > {}; exit 0' TERM\nsleep 30 &\n: > {}\nwait\n",
            trapfile.display(),
            gate.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Parked in `wait` before the gate opens: `sh` defers a trap while a
    // foreground child runs, and a TERM in that child's fork→exec window is lost.
    // No `exec:` clause: exec stays unrestricted, only `fs` is confined —
    // the fixture and `cat` both run inside the grant's envelope, which must
    // cover the temp dir itself: on macOS that is `$TMPDIR`, not `/tmp`.
    let tmp = std::env::temp_dir();
    let script = format!(
        "let job = watch \"tests\" {{ grant [fs: [read: ['{0}'], write: ['{0}']]] {{ {1} | cat }} ; return `done }}\ncat {2}\ncancel $job\n",
        tmp.display(),
        fixture.display(),
        gate.display(),
    );
    let out = run_with_timeout(&[], &script, Duration::from_secs(30));

    let trapped = std::fs::read_to_string(&trapfile);
    std::fs::remove_file(&fixture).ok();
    std::fs::remove_file(&gate).ok();
    std::fs::remove_file(&trapfile).ok();

    let Some(out) = out else {
        panic!("ral never exited: the confined stage's gate never opened");
    };
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(
        trapped.unwrap_or_default().trim(),
        "GRACE",
        "the confined stage's TERM trap never ran: its own envelope group was never signalled"
    );
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
        "within [env: [PATH: '{0}']] {{ grant [exec: [git: 'allow'], fs: [read: ['{0}']]] {{ git }} }}",
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
        "within [env: [PATH: '{0}']] {{ grant [exec: ['{1}': 'allow'], fs: [read: ['{0}']]] {{ git }} }}",
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
    // `resolve_launch` must expand spreads the same way eval_call_args does.
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
fn race_true_producer_does_not_strand_consumer() {
    // `/usr/bin/true` exits immediately with no bytes.  The external, not
    // ral's `true` builtin: the builtin would make stage 1 a ral helper and
    // move the test off the direct-external launch path it is here to
    // exercise.  Both spellings typecheck — a stage promises no traffic.
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
fn pipeline_three_stages_share_anchor_pgid() {
    // Every stage must report the same pgid — the anchor's.
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

    // An external spawned *inside* a ral-written stage thread joins the
    // same anchor pgid as a direct stage in the same pipeline, not one of
    // its own.  `!{ ral --ral-test-pgid-check inner }` forces the first
    // stage to be a ral-written block that spawns the probe as a nested
    // external, rather than launching it direct.
    let nested_script = format!(
        "!{{ {r} --ral-test-pgid-check inner }} | {r} --ral-test-pgid-check outer",
        r = ral.display(),
    );
    let o = run_with_timeout(&[], &nested_script, Duration::from_secs(5))
        .expect("nested-external pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let inner = parse_tagged_pgid(&o.stderr, "inner").expect("inner pgid");
    let outer = parse_tagged_pgid(&o.stderr, "outer").expect("outer pgid");
    assert_eq!(
        inner, outer,
        "external spawned inside a ral stage did not share the anchor pgid; stderr: {}",
        o.stderr
    );
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
    // launch fails after stage 1 has already spawned.  Dropping the
    // half-built `PipeNode` must SIGKILL the pgid before its
    // stage handles join and reap every already-spawned child.  If any
    // child leaks the wait() inside the harness will time out.
    let script = "/usr/bin/true | /no/such/binary_xyzzy | /usr/bin/cat";
    let o = run_with_timeout(&[], script, Duration::from_secs(5))
        .expect("pipeline hung after mid-stage launch failure — child leak?");
    assert_ne!(o.status, 0, "stderr: {}", o.stderr);
}

#[test]
fn pipeline_mid_stage_launch_failure_with_long_producer_kills_it() {
    // Stage 1 is a long-running producer (`yes`); stage 2 fails to launch.
    // The producer must be killed (SIGKILL) by the half-built `PipeNode`'s
    // drop; otherwise it would keep writing to its now-orphaned pipe forever
    // and the test would time out.  This is the canonical Drop-chain regression.
    let script = "/usr/bin/yes | /no/such/binary_xyzzy";
    let o = run_with_timeout(&[], script, Duration::from_secs(5))
        .expect("pipeline hung — long producer not killed on abort?");
    assert_ne!(o.status, 0, "stderr: {}", o.stderr);
}

#[test]
fn race_repeats_deterministically() {
    // `printf ""` exits immediately; the consumer (a ral pgid probe) must
    // still join the pipeline pgid and run to completion, every time.  The
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
// launch: `resolve::resolve_launch` forces such stages through the ral
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
    /// expects on the target.  `.into()` would be `useless_conversion`
    /// on Linux, so the cast lint is the one waived.
    #[allow(clippy::cast_lossless, reason = "per-platform request type")]
    pub unsafe fn become_controlling(fd: RawFd) -> std::io::Result<()> {
        if unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, 0) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A live pty-backed `ral -i --norc` REPL, for tests that need more than one
/// round of input (a line, then a signal, then more input, …).
/// `run_pty_repl_until` is the single-shot case built on top of it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PtySession {
    child: std::process::Child,
    input: std::fs::File,
    reader: std::fs::File,
    bytes: Vec<u8>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl PtySession {
    fn spawn() -> Option<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::CommandExt;

        let pty = pty_helper::open().ok()?;
        let slave_path = pty.slave_path.clone();

        let mut cmd = ral_command();
        cmd.arg("-i").arg("--norc");
        cmd.env("RAL_INTERACTIVE_MODE", "minimal");
        unsafe {
            cmd.pre_exec(move || {
                // New session, then make the pty our controlling terminal
                // and dup it onto fds 0/1/2.  Errors propagate as `execve`-
                // time failures, which the parent sees via `wait`.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let slave = pty_helper::open_slave(&slave_path)?;
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
        let child = cmd.spawn().ok()?;

        let input_fd = unsafe { libc::dup(pty.master.as_raw_fd()) };
        if input_fd < 0 {
            return None;
        }
        let input = unsafe { std::fs::File::from_raw_fd(input_fd) };

        let raw = pty.master.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(raw, libc::F_GETFL);
            libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let reader_fd = unsafe { libc::dup(raw) };
        if reader_fd < 0 {
            return None;
        }
        let reader = unsafe { std::fs::File::from_raw_fd(reader_fd) };

        // Leak the master fd's owner: `input`/`reader` are dup()s of it, and
        // this pty must outlive them for the session's whole life.
        std::mem::forget(pty.master);

        Some(Self {
            child,
            input,
            reader,
            bytes: Vec::new(),
        })
    }

    fn send_line(&mut self, s: &str) -> std::io::Result<()> {
        use std::io::Write;
        writeln!(self.input, "{s}")
    }

    /// Ctrl-Z: 0x1a. Unused below: under this container's virtualized tty
    /// line discipline, writing VSUSP never raises SIGTSTP (`ISIG` is on and
    /// VSUSP maps to 0x1a, but the child stays `S` forever), so a real
    /// SIGTSTP has to be sent to the pipeline's process group directly
    /// instead. Kept for a real tty, where it would exercise ral's
    /// answer-every-stop-with-SIGCONT rule (no job control: Ctrl-Z is
    /// visibly a no-op).
    #[allow(dead_code)]
    fn send_ctrl_z(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.input.write_all(&[0x1a])
    }

    fn pid(&self) -> i32 {
        self.child.id().cast_signed()
    }

    /// Ctrl-C: 0x03.
    fn send_ctrl_c(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.input.write_all(&[0x03])
    }

    fn read_available(&mut self) {
        use std::io::Read;
        let mut chunk = [0u8; 4096];
        loop {
            match self.reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Poll until `ready`, the child exits, or `timeout` elapses, draining
    /// the pty each round so the shell never blocks on a full one.  `true`
    /// only on the `ready` case.
    fn poll_until(&mut self, timeout: Duration, mut ready: impl FnMut(&Self) -> bool) -> bool {
        let start = std::time::Instant::now();
        loop {
            self.read_available();
            if ready(self) {
                return true;
            }
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return false;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Poll until `done` reads true on the accumulated output, the child
    /// exits, or `timeout` elapses.  `true` only on the `done` case.
    fn wait_until(&mut self, timeout: Duration, done: impl Fn(&str) -> bool) -> bool {
        self.poll_until(timeout, |s| done(&s.text()))
    }

    /// The pty's foreground process group, read from the master this test
    /// owns — `tcgetpgrp` answers on the master side even though the caller
    /// belongs to another session.
    fn foreground_pgid(&self) -> Option<i32> {
        rustix::termios::tcgetpgrp(&self.input)
            .ok()
            .map(|pgid| pgid.as_raw_nonzero().get())
    }

    /// Poll until the shell hands the terminal to a pipeline group, and
    /// answer that group — the only honest "the pipeline is up" signal a
    /// test has.  An echoed command line is the *tty's* doing and says
    /// nothing about the shell, which after a fresh link is the best part of
    /// a second from its first prompt; a signal sent before the handover
    /// kills the shell outright (its handlers are not installed until it
    /// boots) or is spared as an idle interrupt at the prompt, but never
    /// reaches the pipeline.
    fn wait_for_lent_terminal(&mut self, timeout: Duration) -> Option<i32> {
        // `spawn` gives the shell a session of its own, so its pgid is its pid.
        let shell = self.pid();
        let mut lent = None;
        self.poll_until(timeout, |s| {
            lent = s.foreground_pgid().filter(|&pgid| pgid != shell);
            lent.is_some()
        });
        lent
    }

    /// Kill the child, drain the pty for a short post-exit window (ral
    /// writes its trace lines just before exiting, and the slave-side fds
    /// may outlive the exit by a few millis on Linux), and reduce to an
    /// `Output`.
    fn finish(mut self) -> Output {
        let _ = self.child.kill();
        let status = self.child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
        let drain_start = std::time::Instant::now();
        while drain_start.elapsed() < Duration::from_millis(200) {
            self.read_available();
            std::thread::sleep(Duration::from_millis(20));
        }
        Output {
            stdout: String::new(),
            stderr: self.text(),
            status,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_pty_repl_until(
    line: &str,
    timeout: Duration,
    done: impl Fn(&str) -> bool,
) -> Option<Output> {
    let mut session = PtySession::spawn()?;
    session.send_line(line).ok()?;
    let reached = session.wait_until(timeout, done);
    let mut out = session.finish();
    out.status = if reached { 0 } else { 124 };
    Some(out)
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn ctrl_c_ends_an_all_ral_foreground_pipeline() {
    // The terminal belongs to the pipeline's group, whose only process is
    // the anchor: the tty's SIGINT reaches nothing that can die, so the
    // anchor must witness it and the collector cancel the stages.
    let mut session = PtySession::spawn().expect("pty setup failed");
    session
        .send_line("!{ let go = { |n| echo tick; go $[$n + 1] }; go 0 } | !{ from-lines }")
        .expect("send failed");
    session
        .wait_for_lent_terminal(Duration::from_secs(8))
        .expect("pipeline never took the terminal");
    session.send_ctrl_c().expect("ctrl-c failed");
    let ended = session.wait_until(Duration::from_secs(8), |t| t.matches("❯").count() >= 2);
    let out = session.finish();
    assert!(
        ended,
        "Ctrl-C did not end an all-ral foreground pipeline; stderr: {}",
        out.stderr
    );
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

// The non-transferable retained-value invariant — that a byte-routed
// helper does not pay for serialising its (unused) return value — is
// covered by `child_eval::tests::stage_job_skips_report_value_when_parent_does_not_need_it`
// at the protocol layer.  Constructing an end-to-end script that
// exercises it is awkward because a byte route pairs with `Unit`, so such
// a helper's body returns nothing worth transferring; the unit test names
// the contract more clearly than any indirect integration shape.

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
fn audited_failing_stage_preserves_nested_external_audit() {
    // A ral stage that runs a nested external and then fails must
    // still leave the nested external's observation in the parent
    // trail.  Pre-fix `unpack_stage_report` discarded observations on
    // structured-error reports.
    //
    // Shape: `printf hi` feeds a ral-written final stage. The block runs
    // `/bin/echo nested-record` (audit-captured by the stage), reads the
    // upstream bytes via `from-string`, and then calls `fail` to report a
    // structured failure.  The parent must extend its audit trail with the
    // nested external before surfacing the stage's error.
    let script = r#"
printf hi | !{ let _s = !{from-string}; let _x = !{/bin/echo nested-record}; fail [status: 1, message: "stage failed"] }
"#;
    let o = run_with_timeout(&["--audit"], script, Duration::from_secs(5))
        .expect("audited failing-stage pipeline hung");
    assert_ne!(o.status, 0, "expected stage failure to bubble up");
    assert!(
        o.stderr.contains("nested-record"),
        "audit must record the nested external even when the stage fails; stderr: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("stage failed"),
        "structured stage error must surface; stderr: {}",
        o.stderr
    );
}

/// Pins: a failing chain arm's bytes flush live, not into the winning arm's
/// decoded value.
#[test]
fn failed_chain_arm_bytes_flush_live_not_into_the_winner() {
    let o = run("let vv = /bin/sh -c 'echo half; exit 3' ? echo x\necho $vv");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "half\nx\n", "stderr: {}", o.stderr);
}

// ── A non-interrupt pipeline cancel must take the whole group down ──────────

/// `pid` is still a live process — signal 0 is the existence probe.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A named pipe whose opening end rendezvous with the fixture, so the test
/// never guesses how long a spawn takes to reach its gate write.
fn mkfifo(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert!(
        unsafe { libc::mkfifo(raw.as_ptr(), 0o600) } == 0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

/// A stage's own kill addresses its pid, never the group; a non-interrupt
/// cancel — here `cancel $job`'s `CancelCause::Explicit` — of a pipeline
/// whose stage forks a grandchild must still take the whole group down, so
/// the collector must signal the group itself.  The `sleep 300 &` the fixture
/// forks never `setsid`s away, so it is the group member that would survive.
/// It also inherits the stage's pumped stderr, so the teardown's group kill
/// is what makes that pump's join terminate.
#[test]
fn a_cancelled_pipeline_stages_grandchild_does_not_survive() {
    let pidfile = fresh_tmp_path("ral_pipeline_cancel_teardown", "pid");
    let gate = fresh_tmp_path("ral_pipeline_cancel_teardown", "gate");
    let fixture = fresh_tmp_path("ral_pipeline_cancel_teardown", "sh");
    mkfifo(&gate);
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\nsleep 300 &\necho $! > {}\n: > {}\nwait\n",
            pidfile.display(),
            gate.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

    // The fixture is a pipeline stage (`| cat`), not a standalone external —
    // `BorrowedByPipeline` is what puts this on the pid-addressed path this
    // regression is about.
    let script = format!(
        "let job = watch \"tests\" {{ {} | cat ; return `done }}\ncat {}\ncancel $job\n",
        fixture.display(),
        gate.display(),
    );
    let out = run_with_timeout(&[], &script, Duration::from_secs(30));

    let recorded = std::fs::read_to_string(&pidfile);
    std::fs::remove_file(&fixture).ok();
    std::fs::remove_file(&gate).ok();
    std::fs::remove_file(&pidfile).ok();

    let Some(out) = out else {
        panic!("ral never exited: the pipeline stage never opened the gate");
    };
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let gc_pid: i32 = recorded
        .expect("the gate opened without the grandchild pid being published")
        .trim()
        .parse()
        .expect("a pid");

    // The host's drain grace (worker registry's `WORKER_DRAIN_GRACE`) is the
    // outer bound; this margin is for a loaded machine's scheduling.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline && alive(gc_pid) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = alive(gc_pid);
    if survived {
        unsafe { libc::kill(gc_pid, libc::SIGKILL) };
    }
    assert!(
        !survived,
        "the forked grandchild (pid {gc_pid}) survived the pipeline's cancel teardown"
    );
}

/// Every other cancellation fixture honours `SIGTERM`, so nothing else in the
/// suite reaches the branch *after* a grace deadline expires.  This one covers
/// the stage's own: `RunningChild::terminate` signals the pid, then blocks on
/// one `recv_timeout(TEARDOWN_GRACE)`, and the `SIGKILL` that follows is what
/// ends a stage that blocks in the shell itself — no child for the signal to
/// fell instead.
#[test]
fn a_cancelled_stage_that_ignores_sigterm_dies_when_the_grace_expires() {
    let gate = fresh_tmp_path("ral_pipeline_grace_expiry", "gate");
    let blocker = fresh_tmp_path("ral_pipeline_grace_expiry", "blocker");
    let fixture = fresh_tmp_path("ral_pipeline_grace_expiry", "sh");
    mkfifo(&gate);
    mkfifo(&blocker);
    // `read` is a builtin, so sh blocks opening a fifo nobody ever writes:
    // the ignored TERM reaches the process that is actually waiting.
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\ntrap \"\" TERM\n: > {}\nread x < {}\n",
            gate.display(),
            blocker.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

    let script = format!(
        "let job = watch \"tests\" {{ {} | cat ; return `done }}\ncat {}\ncancel $job\n",
        fixture.display(),
        gate.display(),
    );
    let started = std::time::Instant::now();
    let out = run_with_timeout(&[], &script, Duration::from_secs(30));
    let elapsed = started.elapsed();

    std::fs::remove_file(&fixture).ok();
    std::fs::remove_file(&gate).ok();
    std::fs::remove_file(&blocker).ok();

    let Some(out) = out else {
        panic!("ral never exited: the cancelled stage outlived the teardown grace");
    };
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        elapsed < Duration::from_secs(10),
        "the teardown paid far more than one grace: {elapsed:?}"
    );
}

/// The group's grace, and the reason the kill precedes the join.  Like
/// `a_cancelled_pipeline_stages_grandchild_does_not_survive`, but the
/// grandchild ignores `SIGTERM`, so the polite group signal does not fell it
/// and the stage's own kill — pid-addressed, the stage being
/// `BorrowedByPipeline` — cannot reach it.  It holds the stage's pumped stderr
/// open, so nothing but the group `SIGKILL` after `TEARDOWN_GRACE` lets the
/// pump's join return.  Take that kill out of `cancel_all` and the grandchild
/// outlives the pipeline: `Drop`'s follow-up is too late to be observed.
#[test]
fn a_cancelled_stages_sigterm_proof_grandchild_dies_when_the_group_grace_expires() {
    let pidfile = fresh_tmp_path("ral_pipeline_group_grace", "pid");
    let gate = fresh_tmp_path("ral_pipeline_group_grace", "gate");
    let blocker = fresh_tmp_path("ral_pipeline_group_grace", "blocker");
    let fixture = fresh_tmp_path("ral_pipeline_group_grace", "sh");
    mkfifo(&gate);
    mkfifo(&blocker);
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\nsh -c 'trap \"\" TERM; read x < {}' &\necho $! > {}\n: > {}\nwait\n",
            blocker.display(),
            pidfile.display(),
            gate.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

    let script = format!(
        "let job = watch \"tests\" {{ {} | cat ; return `done }}\ncat {}\ncancel $job\n",
        fixture.display(),
        gate.display(),
    );
    let started = std::time::Instant::now();
    let out = run_with_timeout(&[], &script, Duration::from_secs(30));
    let elapsed = started.elapsed();

    let recorded = std::fs::read_to_string(&pidfile);
    std::fs::remove_file(&fixture).ok();
    std::fs::remove_file(&gate).ok();
    std::fs::remove_file(&blocker).ok();
    std::fs::remove_file(&pidfile).ok();

    let Some(out) = out else {
        panic!("ral never exited: the collector joined a pump the grandchild still holds open");
    };
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        elapsed < Duration::from_secs(10),
        "the teardown paid far more than one grace: {elapsed:?}"
    );

    let gc_pid: i32 = recorded
        .expect("the gate opened without the grandchild pid being published")
        .trim()
        .parse()
        .expect("a pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline && alive(gc_pid) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = alive(gc_pid);
    if survived {
        unsafe { libc::kill(gc_pid, libc::SIGKILL) };
    }
    assert!(
        !survived,
        "the SIGTERM-proof grandchild (pid {gc_pid}) outlived the group's grace"
    );
}
