#![allow(clippy::disallowed_methods)]
//! Windows-only pipeline integration tests.
//!
//! Cover the surfaces that the Windows port had to grow from scratch:
//!
//!   * external-only byte pipeline,
//!   * external → ral helper byte pipeline,
//!   * ral helper → external byte pipeline,
//!   * a block literal in stage position runs nothing,
//!   * final value returned from a helper,
//!   * `2>&1` inside a pipeline stage,
//!   * stage redirect inside a pipeline (`cmd > file`),
//!   * missing-command diagnostic surfaces the user's command name,
//!   * an unforced block cannot fail, because it never runs,
//!   * a consumer that never reads closes its read end, so an unbounded
//!     producer dies of a broken pipe instead of filling one forever,
//!   * a leader exiting before later stages does not reap the pipeline
//!     prematurely (whole-job completion),
//!
//! Tests are gated on `#[cfg(windows)]` and run only on Windows hosts.
//! On other platforms the file compiles to nothing so cross-builds stay
//! clean.

#![cfg(windows)]

mod common;

use common::{run, run_with_env, run_with_timeout};
use std::time::Duration;

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
        to-lines [a, b, c, ""] | findstr /N .
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(out.stdout.contains('a'), "stdout={}", out.stdout);
    assert!(out.stdout.contains('c'), "stdout={}", out.stdout);
}

/// A block literal in stage position is an ordinary value: the stage
/// returns a thunk and runs nothing.  The upstream list is never
/// serialised onto the wire either — a returned value in non-final
/// position is simply discarded — so the bound result is the thunk, not
/// the `3` the block would have computed had anything forced it.
#[test]
fn block_literal_stage_returns_a_thunk_and_runs_nothing() {
    let out = run(
        "win_pipeline_thunk_stage",
        r"
        let res = !{ [1, 2, 3] | { |xs| return !{length $xs} } }
        echo $res
        ",
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(
        !out.stdout.contains('3'),
        "the block was forced; stdout={}",
        out.stdout
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
/// overrides the pipeline's downstream pipe for that stage, so the next
/// stage reads EOF and the file holds the producer's output.
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

/// The other half of the same rule, read off the status rather than the
/// value: an unforced block never runs, so the `fail` inside it never
/// fires.  Both stages are thunks nobody forces, and the pipeline — whose
/// own value is discarded at statement position — succeeds having done
/// nothing at all.
#[test]
fn a_failing_block_in_stage_position_never_fires() {
    let out = run(
        "win_pipe_unforced_fail",
        r#"
        { fail [status: 1, message: "boom"] } | { |x| echo $x }
        "#,
    );
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert!(
        out.stdout.is_empty(),
        "an unforced block wrote stdout={}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("boom"),
        "the unforced block's failure fired; stderr={}",
        out.stderr
    );
}

/// Neither side of a `|` promises traffic, and the producer's side of that
/// symmetry is the one with teeth: `for /L %i in (1,0,2)` steps by zero and
/// so never stops writing, while the consumer returns without touching
/// stdin.  Finishing must close the consumer's read end promptly, or the
/// producer blocks on a full pipe nobody will ever drain.  The pipeline's
/// value is the consumer's own `5`; not one byte of the firehose is in it.
#[test]
fn a_firehose_into_a_non_reading_consumer_terminates() {
    let out = run_with_timeout(
        "win_pipe_firehose",
        &[],
        r#"
        let n = !{ cmd /c "for /L %i in (1,0,2) do @echo DATA" | !{ return 5 } }
        echo $n
        "#,
        Duration::from_secs(20),
    )
    .expect("firehose hung — the consumer's read end outlived the consumer");
    assert_eq!(out.status, 0, "stderr={}", out.stderr);
    assert_eq!(out.stdout.trim(), "5", "stdout={}", out.stdout);
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
