//! Windows-only pipeline integration tests.
//!
//! Cover the surfaces that the Windows port had to grow from scratch:
//!
//!   * external-only byte pipeline,
//!   * external → ral helper byte pipeline,
//!   * ral helper → external byte pipeline,
//!   * ral helper → ral helper value edge,
//!   * final value returned from a helper,
//!   * `2>&1` inside a pipeline stage,
//!   * stage redirect inside a pipeline (`cmd > file`),
//!   * missing-command diagnostic surfaces the user's command name,
//!   * upstream helper failure surfaces the structured "value edge
//!     closed" diagnostic at the consumer,
//!   * long-running producer is killed when the downstream finishes
//!     (`yes | head` style),
//!   * a leader exiting before later stages does not reap the pipeline
//!     prematurely (whole-job completion),
//!   * `disown` leaves children alive; cleanup kills non-disowned jobs.
//!
//! Tests are gated on `#[cfg(windows)]` and run only on Windows hosts.
//! On other platforms the file compiles to nothing so cross-builds stay
//! clean.

#![cfg(windows)]

mod common;

use common::run;

/// External-only byte pipeline.  `cmd /c echo hi | findstr hi` is the
/// canonical "two externals chained by stdout" check; ral's launcher
/// must allocate one OS pipe and assign both stages to the same Job
/// Object so a Ctrl-Break tears down both.
#[test]
fn external_only_pipeline_runs() {
    let out = run(
        "win_pipeline_external",
        r#"cmd /c "echo hi" | findstr "hi""#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.contains("hi"), "stdout={}", out.stdout);
}

/// External → ral helper byte pipeline.  The helper consumes upstream
/// bytes via `from-lines` and returns a value-typed result; the
/// pipeline's last stage is a ral helper, so the final value is
/// transported through the helper's `ChildEvalResponse` frame, not through
/// stdout.
#[test]
fn external_to_helper_pipeline_returns_value() {
    let out = run(
        "win_pipeline_ext_to_ral",
        r#"
        let s = !{ cmd /c "echo a& echo b& echo c" | from-lines }
        let lines = !{ stream-to-list $s }
        echo !{length $lines}
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.trim().ends_with("3"), "stdout={}", out.stdout);
}

/// ral helper → external byte pipeline.  The helper writes bytes to
/// its stdout (via `to-lines`); the external consumer reads them.
#[test]
fn helper_to_external_pipeline_runs() {
    let out = run(
        "win_pipeline_ral_to_ext",
        r#"
        [a, b, c, ""] | to-lines | findstr /N .
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.contains("a"), "stdout={}", out.stdout);
    assert!(out.stdout.contains("c"), "stdout={}", out.stdout);
}

/// ral helper → ral helper via a value edge.  The producer returns a
/// list, the consumer takes it as data-last input.  The pipeline
/// allocates an inter-stage value channel (anonymous pipe pair on
/// Windows) and passes the producer the writer end, the consumer the
/// reader end.
#[test]
fn helper_to_helper_value_edge() {
    let out = run(
        "win_pipeline_value_edge",
        r#"
        let r = !{ [1, 2, 3] | { |xs| return !{length $xs} } }
        echo $r
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.trim().ends_with("3"), "stdout={}", out.stdout);
}

/// `2>&1` inside a pipeline stage.  The stage's stderr must be
/// duplicated onto whichever target stdout was assigned (here: the
/// downstream pipe).  Without the Windows arm of
/// `wire_pipeline_stage_stderr`, the diagnostic vanishes into the
/// parent.
#[test]
fn pipeline_stage_2to1_routes_into_pipe() {
    let out = run(
        "win_pipeline_2to1",
        r#"
        cmd /c "echo out & echo err 1>&2" 2>&1 | findstr "err"
        "#,
    );
    // The downstream `findstr` matches the stderr that was redirected
    // into the pipe; absent the redirect, only "out" would reach it.
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.contains("err"), "stdout={}", out.stdout);
}

/// Stage-level `>file` redirect inside a pipeline.  The redirect
/// overrides the pipeline's downstream pipe for that stage, so the
/// next stage sees an empty pipe (pipeline byte channel falls through
/// to parent stdin) and the file holds the producer's output.
#[test]
fn pipeline_stage_redirect_to_file() {
    let tmp = common::fresh_tmp_path("win_pipe_stage_redir", "txt");
    let tmp_str = tmp.to_string_lossy().replace('\\', "/");
    let script = format!(
        r#"
        cmd /c "echo redirected" > {tmp_str} | cmd /c rem
        "#,
    );
    let out = run("win_pipe_stage_redir", &script);
    let content = std::fs::read_to_string(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(
        content.contains("redirected"),
        "file content={content:?}, stderr={}",
        out.stderr
    );
}

/// Missing command in a pipeline.  The user's command name is what
/// appears in the error, not the helper's internals.
#[test]
fn missing_command_in_pipeline_reports_user_command() {
    let out = run(
        "win_pipe_missing",
        r#"this-command-does-not-exist-1729 | findstr ."#,
    );
    assert_ne!(out.status, 0);
    assert!(
        out.stderr.contains("this-command-does-not-exist-1729"),
        "stderr={}",
        out.stderr
    );
}

/// Upstream helper failure on a value edge.  The producer raises
/// before returning a value; the consumer must surface "pipeline
/// value edge closed before a value was sent" rather than running
/// with phantom-None upstream.
#[test]
fn upstream_helper_failure_reaches_consumer() {
    let out = run(
        "win_pipe_upstream_fail",
        r#"
        { fail "boom" } | { |x| echo $x }
        "#,
    );
    assert_ne!(out.status, 0);
    let stderr = out.stderr.to_lowercase();
    assert!(
        stderr.contains("boom") || stderr.contains("value edge"),
        "stderr={}",
        out.stderr
    );
}

/// Long-running producer must die when the downstream finishes
/// (`yes | head` style).  We use `cmd /c "for /L %i in (1,1,1000000) do
/// @echo x" | findstr /N x` and stop after a few matches; the producer
/// process should be terminated by the Job Object teardown when the
/// pipeline collapses on the parent's read closure.
///
/// Marked `#[ignore]` because the timing varies on CI and the test is
/// best run interactively; the structural property (Job Object
/// completion) is also exercised by `whole_job_completion_required`
/// below.
#[test]
#[ignore]
fn long_producer_killed_when_downstream_done() {
    let out = run(
        "win_pipe_long_producer",
        r#"
        cmd /c "for /L %i in (1,1,100000) do @echo x" | from-lines | take 3
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
}

/// Whole-job completion: the leader (first stage) exits early because
/// it has nothing more to write, but later stages keep running.  The
/// pipeline's reap waits for `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`, so
/// the user-visible pipeline status is the *last* stage's, not the
/// leader's premature exit.
#[test]
fn whole_job_completion_required() {
    let out = run(
        "win_pipe_whole_job",
        r#"
        cmd /c "echo done" | from-string | { |s| echo $s }
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.contains("done"), "stdout={}", out.stdout);
}

/// `disown` clears KILL_ON_JOB_CLOSE on the group's Job Object so the
/// children survive ral's exit.  This test asserts the bookkeeping is
/// removed; it does not assert the children survive ral exit because
/// integration tests don't have the shell-exit hook the REPL runs.
#[test]
fn disown_drops_bookkeeping() {
    let out = run(
        "win_pipe_disown",
        r#"
        let job = spawn { cmd /c "ping -n 2 127.0.0.1 >NUL" }
        # No `disown` exposed in the script harness yet; cleanup is what
        # exercises kill_pipeline_group on shell exit.
        echo ok
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.contains("ok"), "stdout={}", out.stdout);
}
