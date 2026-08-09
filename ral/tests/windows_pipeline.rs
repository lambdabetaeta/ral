#![allow(clippy::disallowed_methods)]
//! Windows-only pipeline integration tests.
//!
//! Cover the surfaces that the Windows port had to grow from scratch:
//!
//!   * external-only byte pipeline,
//!   * external → ral helper byte pipeline,
//!   * ral helper → external byte pipeline,
//!   * rejection of an implicit value edge,
//!   * final value returned from a helper,
//!   * `2>&1` inside a pipeline stage,
//!   * stage redirect inside a pipeline (`cmd > file`),
//!   * missing-command diagnostic surfaces the user's command name,
//!   * a value-producing helper is rejected before any consumer runs,
//!   * a leader exiting before later stages does not reap the pipeline
//!     prematurely (whole-job completion),
//!
//! Tests are gated on `#[cfg(windows)]` and run only on Windows hosts.
//! On other platforms the file compiles to nothing so cross-builds stay
//! clean.

#![cfg(windows)]

mod common;

use common::{run, run_with_env};

/// A file in the directory the user just `cd`'d into is not on `PATH`, and a
/// `PATH` ending in `;` — the ubiquitous Windows spelling — does not put it
/// there.  Naming it bare is "command not found", 127; never a synthesised
/// "permission denied" about a program no walk ever resolved.
#[test]
fn bare_cwd_file_reports_command_not_found() {
    let dir = common::fresh_tmp_path("win_cwd_bare", "d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("zzcmd.bat"), "@echo IT-RAN\r\n").unwrap();
    let inherited = std::env::var("PATH").unwrap_or_default();

    let out = run_with_env(
        "win_cwd_bare",
        &[("PATH", &format!("{inherited};"))],
        &format!("cd '{}'\nzzcmd.bat\n", dir.display()),
    );
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(out.status, 127, "stderr={}", out.stderr);
    assert!(
        !out.stderr.to_lowercase().contains("permission denied"),
        "stderr={}",
        out.stderr,
    );
    assert!(
        out.stderr.contains("command not found"),
        "stderr={}",
        out.stderr,
    );
}

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
    assert!(out.stdout.trim().ends_with('3'), "stdout={}", out.stdout);
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
    assert!(out.stdout.contains('a'), "stdout={}", out.stdout);
    assert!(out.stdout.contains('c'), "stdout={}", out.stdout);
}

/// A value-producing helper cannot feed another stage as an implicit
/// argument.  The checker rejects the program before either helper runs.
#[test]
fn helper_to_helper_value_edge_is_rejected() {
    let out = run(
        "win_pipeline_value_edge",
        r"
        let res = !{ [1, 2, 3] | { |xs| return !{length $xs} } }
        echo $res
        ",
    );
    assert_ne!(out.status, 0, "expected a compile error");
    assert!(
        out.stdout.is_empty(),
        "rejected pipeline wrote stdout={}",
        out.stdout
    );
    assert!(
        out.stderr.contains("this stage produces a value"),
        "stderr={}",
        out.stderr
    );
}

/// `2>&1` inside a pipeline stage.  The stage's stderr must be
/// duplicated onto whichever target stdout was assigned (here: the
/// downstream pipe).  Without the Windows arm of `wire_stage_stdio`,
/// the diagnostic vanishes into the parent.
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
    // Quoted: a Windows temp dir is often an 8.3 short path
    // (`C:/Users/RUNNER~1/...`), and a bare word ends at the `~` that
    // opens a `~user` path.
    let script = format!(
        r#"
        cmd /c "echo redirected" > '{tmp_str}' | cmd /c rem
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
        r"this-command-does-not-exist-1729 | findstr .",
    );
    assert_ne!(out.status, 0);
    assert!(
        out.stderr.contains("this-command-does-not-exist-1729"),
        "stderr={}",
        out.stderr
    );
}

/// A failing value-producing helper is rejected as a value pipeline before
/// its failure or the consumer's side effect can occur.
#[test]
fn upstream_value_helper_is_rejected_before_consumer() {
    let out = run(
        "win_pipe_upstream_fail",
        r#"
        { fail [status: 1, message: "boom"] } | { |x| echo $x }
        "#,
    );
    assert_ne!(out.status, 0);
    assert!(
        out.stdout.is_empty(),
        "rejected pipeline wrote stdout={}",
        out.stdout
    );
    let stderr = out.stderr.to_lowercase();
    assert!(
        stderr.contains("this stage produces a value"),
        "stderr={}",
        out.stderr
    );
}

/// Whole-job completion: the leader (first stage) exits early because
/// it has nothing more to write, but later stages keep running.  The
/// pipeline's reap waits for `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`, so
/// the user-visible pipeline status is the *last* stage's, not the
/// leader's premature exit.
#[test]
fn whole_job_completion_required() {
    // The tail echoes what it *read*, so the assertion still witnesses the
    // leader's bytes arriving rather than a literal the tail could print on
    // its own.
    let out = run(
        "win_pipe_whole_job",
        r#"
        let relay = {
            let s = !{from-line}
            echo $s
        }
        cmd /c "echo done" | !$relay
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.contains("done"), "stdout={}", out.stdout);
}
