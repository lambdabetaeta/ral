//! Pipeline mechanics: process groups, signal routing, broken pipes,
//! concurrent spawned pipelines, and output correctness.
//!
//! All tests run ral as a subprocess.  Signal relay and tcsetpgrp are only
//! active in the interactive shell (`is_interactive=true`); these tests
//! exercise the batch-mode plumbing — process group setup, pipe wiring,
//! exit-status propagation — which is shared with the interactive shell.
//!
//! Unix-only: these tests rely on Unix commands (/bin/echo, grep, cat, yes,
//! head, wc) and Unix process-group / signal semantics.  What needs no harness
//! — stage dispatch, redirects, statuses, capture rules — is the golden
//! `tests/run/unix/pipeline-stages.ral` and `tests/run/lang/pipeline-values.ral`.

#![cfg(unix)]

use crate::common::{Output, fresh_tmp_path, ral_command};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

mod terminal;

pub(crate) fn run(script: &str) -> Output {
    crate::common::run("ral_test", script)
}

pub(crate) fn run_with_timeout(args: &[&str], script: &str, timeout: Duration) -> Option<Output> {
    crate::common::run_with_timeout("ral_test", args, script, timeout)
}

// ── All-external pipelines ───────────────────────────────────────────────────

#[cfg(feature = "ripgrep")]
#[test]
fn external_pipeline_bundled_rg() {
    let o = run("/bin/echo hello | rg hello");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hello");
}

#[test]
fn external_pipeline_argument_errors_are_not_dropped() {
    let o = run("/bin/echo !{fail argboom} | cat");
    assert_ne!(o.status, 0);
    assert!(o.stderr.contains("argboom"), "stderr: {}", o.stderr);
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
    // The audit dump is compact JSON; the captured stdout bytes are rendered as a lossy-UTF-8 string
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

// ── Stage dispatch parity (handlers, ^name, redirects) ─────────────────────
//
// These regressions cover the rule that pipeline-stage dispatch must match
// the `Exec` rule's: a `within [handlers: …]` interception of an external
// name must fire even mid-pipeline, `^name` must skip binding lookup
// (pipeline included), and stage-level redirects must be honored rather than
// silently dropped.

#[test]
fn pipeline_external_stage_rejects_list_arg_with_hint() {
    // Passing a List as a positional arg to an external stage must error
    // with the same diagnostic command::run produces — `...$xs` hint.
    // The shared command::validate_argv path is what enforces this — pipeline
    // stages and single-command exec both run vet before spawn, so a
    // list arg cannot reach `execve` as a (likely garbled) stringification.
    //
    // The list arrives decoded rather than written, so that this is the vet
    // path and not the checker's: `from-json` returns a value of no particular
    // type, and only the run knows it is a list.
    let o = run("let xs = /bin/echo '[1, 2, 3]' | from-json; /bin/echo hi | /usr/bin/printf $xs");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr
            .contains("cannot pass a list as argument at index 0 to `/usr/bin/printf`"),
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
        "let xs = !{cat | from-lines}; echo done; echo !{length $xs}",
        Duration::from_secs(5),
    )
    .expect("mixed-pipeline first external stage hung — likely inherited stdin");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
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
// dead write is certain, and a producer that never writes again is never cut
// at all.

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

// ── Stress: many sequential pipelines ───────────────────────────────────────

#[test]
fn many_sequential_pipelines_complete() {
    let script = r"
let _go = { |n|
    if $[$n <= 0] {} else {
        /bin/echo $n | !{filter-lines { |_| true }} | grep . > /dev/null
        _go $[$n - 1]
    }
}
_go 10
echo done
";
    let o = run_with_timeout(&[], script, Duration::from_mins(1))
        .expect("sequential pipeline stress timed out");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "done\n");
}

// ── Exit-status classification and group edge cases ──────────────────────────

#[test]
fn a_nested_pipeline_joins_its_stages_group() {
    // A pipeline launched from inside a ral-written stage thread must join
    // the outer pipeline's group rather than prepare its own: every stage
    // of `inner1 | inner2`, run from within the outer's first stage, and
    // the outer's own direct final stage, all share one anchor pgid.  The
    // final stage drains its stdin first, so the inner stages have already
    // reported and exited before their edge could ever die under them.
    let script = format!(
        "!{{ {} | {} }} | sh -c 'cat >/dev/null; {}'",
        probe("inner1"),
        probe("inner2"),
        report("outer"),
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
/// exactly ral, not the test runner — `SIGINT` that group once `ready` says
/// the pipeline is running, and return how ral ended within 3 s: `None` if it
/// did not.  In batch mode the shell itself receives the SIGINT and cancels
/// the run, so it must *exit* — a status with no code means a signal killed
/// it (a `SIGPIPE` from its own interior edge, say).
fn ral_exits_after_sigint(
    script: &str,
    ready: impl FnOnce() -> bool,
) -> Option<std::process::ExitStatus> {
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
    let started = ready();
    let group = -child.id().cast_signed();
    unsafe { libc::kill(group, if started { libc::SIGINT } else { libc::SIGKILL }) };
    assert!(started, "the pipeline never started");
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

fn assert_ral_exited_after_sigint(script: &str, ready: impl FnOnce() -> bool, hang: &str) {
    let status = ral_exits_after_sigint(script, ready).unwrap_or_else(|| panic!("{hang}"));
    assert!(
        status.code().is_some(),
        "ral was killed by a signal instead of exiting: {status}"
    );
}

/// A fresh, absent marker path for a script to touch once its pipeline runs.
fn ready_marker() -> std::path::PathBuf {
    fresh_tmp_path("ral_sigint", "ready")
}

/// Poll until `marker` exists, then remove it; `false` after 10 s.
fn marker_appears(marker: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_file(marker).ok();
    true
}

#[test]
fn sigint_kills_external_child_in_pipeline() {
    // The externals have SIG_DFL and die of the relayed signal.
    let ready = ready_marker();
    assert_ral_exited_after_sigint(
        &format!(
            "/bin/echo start | sh -c #': > {}; exec sleep 60'#\n",
            ready.display()
        ),
        || marker_appears(&ready),
        "ral did not exit after SIGINT",
    );
}

#[test]
fn sigint_kills_a_ral_stage_blocked_reading_a_live_upstream() {
    // A stage thread blocked in `from-line` on a producer that never
    // writes: only the wake can end its read.
    let ready = ready_marker();
    assert_ral_exited_after_sigint(
        &format!(
            "sleep 100 | !{{ touch '{}'; from-line }}\n",
            ready.display()
        ),
        || marker_appears(&ready),
        "a stage thread blocked on a live upstream was not woken",
    );
}

#[test]
fn sigint_kills_a_ral_stage_blocked_writing_to_a_full_edge() {
    // The producer fills the edge (well under a second) and blocks in
    // `echo`.  Only the wake may end that write: the parent holds the edge's
    // read end, and an `EPIPE` there would be a `SIGPIPE` to the whole shell.
    // No marker can mark "full", so this one keeps a settle span.
    assert_ral_exited_after_sigint(
        "!{ let go = { |n| echo tick; go $[$n + 1] }; go 0 } | sleep 100\n",
        || {
            std::thread::sleep(Duration::from_secs(1));
            true
        },
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

// ── Foreground-handoff race regressions ─────────────────────────────────────
//
// The pipeline anchor (a tiny null-stdio ral helper) keeps the pgid alive
// even when the first stage exits before later stages spawn.  These tests
// exercise that invariant with a [`probe`] stage reporting its pgid.  In a
// pre-anchor build, a fast producer like `printf ""` could let the pgid go
// away before the consumer joined it, stranding the consumer; the assertions
// below would either time out or observe inconsistent pgids.

/// A `sh` body writing `pgid:<tag>=… tcpgrp:<tag>=…` to stderr in one line,
/// the tty's foreground group being `0` or `-1` when there is none.  The tag
/// goes through `$t` because a pty echoes the typed line, which must not
/// itself match.  Needs a `ps` with `tpgid` (procps, BSD); busybox has none.
fn report(tag: &str) -> String {
    format!("t={tag}; set -- $(ps -o pgid=,tpgid= -p $$); echo \"pgid:$t=$1 tcpgrp:$t=$2\" >&2")
}

/// A stage that [`report`]s, then drains its stdin: a reader exiting before
/// its writer is done makes ral end that writer (SPEC §7.6).
pub(crate) fn probe(tag: &str) -> String {
    format!("sh -c '{}; cat >/dev/null'", report(tag))
}

pub(crate) fn parse_tagged_pgid(stderr: &str, tag: &str) -> Option<i32> {
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
    let script = format!("/usr/bin/true | {}", probe("post"));
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
    let script = format!("{} | {} | {}", probe("a"), probe("b"), probe("c"));
    let o = run_with_timeout(&[], &script, Duration::from_secs(5)).expect("pipeline hung");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let a = parse_tagged_pgid(&o.stderr, "a").expect("a pgid");
    let b = parse_tagged_pgid(&o.stderr, "b").expect("b pgid");
    let c = parse_tagged_pgid(&o.stderr, "c").expect("c pgid");
    assert_eq!(a, b, "stages a/b differ; stderr: {}", o.stderr);
    assert_eq!(b, c, "stages b/c differ; stderr: {}", o.stderr);

    // An external spawned *inside* a ral-written stage thread joins the
    // same anchor pgid as a direct stage in the same pipeline, not one of
    // its own.  `!{ probe }` forces the first stage to be a ral-written
    // block that spawns the probe as a nested external, rather than
    // launching it direct.
    let nested_script = format!("!{{ {} }} | {}", probe("inner"), probe("outer"));
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
fn pipeline_mid_stage_launch_failure_with_long_producer_kills_it() {
    // Stage 1 is a long-running producer (`yes`); stage 2 fails to launch.
    // The failed launch's rollback must tear the producer down; otherwise it
    // would keep writing to its orphaned pipe forever and the test would time
    // out.
    let script = "/usr/bin/yes | /no/such/binary_xyzzy";
    let o = run_with_timeout(&[], script, Duration::from_secs(5))
        .expect("pipeline hung — long producer not killed on abort?");
    assert_ne!(o.status, 0, "stderr: {}", o.stderr);
}

#[test]
fn pipeline_path_literal_exec_failure_reports_127() {
    // `/no/such/binary` cannot be spawned.  Direct external launch
    // rebuilds that as `Failure::NotFound` against the
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
    // A non-executable file in a pipeline is refused with exit 126 — the
    // POSIX status for "found but not executable" — and named as a file.
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
        o.stderr.contains("is a file, not an executable"),
        "must say what the path is; stderr: {}",
        o.stderr
    );
}

// ── Audit + redirect: no panic ───────────────────────────────────────────────
//
// A stage with `> file` (or `2> file`, or `> file 2>&1`) under `--audit`
// swaps its sink inside the audit tee, so the bytes reach the file and the
// audit records none.

#[test]
fn audited_stdout_and_stderr_redirect_does_not_panic() {
    // `> file 2>&1` joins both streams into the same file.
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
printf hi | !{ let _s = !{from-string}; let _x = !{/bin/echo nested-record}; fail "stage failed" }
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

// ── A non-interrupt pipeline cancel must take the whole group down ──────────

/// `pid` is still a live process — signal 0 is the existence probe.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A named pipe whose opening end rendezvous with the fixture, so the test
/// never guesses how long a spawn takes to reach its gate write.
pub(crate) fn mkfifo(path: &std::path::Path) {
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
/// cancel — here `cancel $job`'s `CancelCause::Cancelled` — of a pipeline
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
/// the collector's: `cancel_all` signals, then waits out `TEARDOWN_GRACE`,
/// and the `SIGKILL` that follows is what ends a stage that blocks in the
/// shell itself — no child for the signal to fell instead.
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

// ── A stage's stops and escapes: who consumes them, what the fold ranks ──────

/// Run `script(marker)` and return how it ended and whether it touched the
/// marker; the marker is removed.
fn run_marked(tag: &str, script: impl FnOnce(&str) -> String, timeout: Duration) -> (Output, bool) {
    let path = fresh_tmp_path(tag, "mark");
    let _ = std::fs::remove_file(&path);
    let o = run_with_timeout(&[], &script(&format!("'{}'", path.display())), timeout)
        .unwrap_or_else(|| panic!("{tag}: pipeline hung"));
    let touched = path.exists();
    let _ = std::fs::remove_file(&path);
    (o, touched)
}

#[test]
fn a_producers_try_cannot_change_its_reader_gone_stop() {
    // A ral writer, whose dead write is the stop; an external's death is
    // forgiven before any `try` could see it.
    let (o, handled) = run_marked(
        "ral_pipe_try_stop",
        |f| {
            format!(
                "!{{ try {{ each {{ |i| echo $i }} !{{range 1 1000000}} }} {{ |e| touch {f} }} }} | head -1"
            )
        },
        Duration::from_secs(10),
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "1\n");
    assert!(!handled, "a try handler saw the reader-gone stop");
}

#[test]
fn a_reader_gone_stage_runs_its_cleanup_and_the_cleanups_halt_is_the_verdict() {
    let (o, ran) = run_marked(
        "ral_pipe_gone_cleanup_halt",
        |f| format!("!{{ guard {{ () }} {{ |_| yes }} {{ |_ _| touch {f}; fail x }} }} | head -1"),
        Duration::from_secs(10),
    );
    assert_eq!(o.status, 1, "stderr: {}", o.stderr);
    assert!(ran, "the cleanup did not run");
}

#[test]
fn a_reader_gone_stages_cleanup_that_ends_on_it_is_forgiven() {
    let (o, ran) = run_marked(
        "ral_pipe_gone_cleanup_forgiven",
        |f| {
            format!(
                "!{{ guard {{ () }} {{ |_| yes }} {{ |_ _| touch {f}; echo tail }} }} | head -1"
            )
        },
        Duration::from_secs(10),
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(ran, "the cleanup did not run");
}

#[test]
fn a_nested_reader_gone_stages_cleanup_halt_is_the_verdict() {
    // The inner fold's teardown is no verdict, a stage having halted; the
    // outer collector forgives nothing it was not handed.
    let o = run_with_timeout(
        &[],
        "!{ !{ guard { () } { |_| yes } { |_ _| fail x } } | cat } | head -1",
        Duration::from_secs(10),
    )
    .expect("nested pipeline hung");
    assert_eq!(o.status, 1, "stderr: {}", o.stderr);
}

#[test]
fn a_reader_gone_stages_cleanup_write_is_stopped_and_its_tail_discarded() {
    let (o, ran) = run_marked(
        "ral_pipe_gone_cleanup_write",
        |f| {
            format!(
                "!{{ guard {{ () }} {{ |_| yes }} {{ |_ _| echo tail; touch {f} }} }} | head -1"
            )
        },
        Duration::from_secs(10),
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        !ran,
        "the cleanup continued past its write to the dead edge"
    );
}

/// More than any pipe holds: `head` may grow its own to 1 MiB.
const PAST_ANY_PIPE: &str = "/bin/sh -c 'head -c 8000000 /dev/zero'";

/// `run_marked`, and how long the run took.
fn run_marked_timed(tag: &str, script: impl FnOnce(&str) -> String) -> (Output, bool, Duration) {
    let started = std::time::Instant::now();
    let (o, touched) = run_marked(tag, script, Duration::from_secs(15));
    (o, touched, started.elapsed())
}

#[test]
fn a_reader_gone_stages_cleanup_command_runs_to_completion_and_its_halt_is_the_verdict() {
    // The cut the shield masks once left it blocked until the 5 s grace.
    let (o, ran, took) = run_marked_timed("ral_pipe_gone_cleanup_command", |f| {
        format!(
            "!{{ guard {{ () }} {{ |_| /bin/sleep 0.2 }} {{ |_ _| {PAST_ANY_PIPE}; touch {f}; fail x }} }} | !{{ return () }}"
        )
    });
    assert_eq!(o.status, 1, "stderr: {}", o.stderr);
    assert!(ran, "the cleanup's command never finished");
    assert!(
        took < Duration::from_secs(3),
        "it waited out the grace: {took:?}"
    );
}

#[test]
fn a_torn_down_stages_cleanup_does_not_block_on_its_escaped_reader() {
    let (o, ran, took) = run_marked_timed("ral_pipe_teardown_cleanup_command", |f| {
        format!(
            "!{{ guard {{ () }} {{ |_| /bin/sleep 5 }} {{ |_ _| {PAST_ANY_PIPE}; touch {f} }} }} | !{{ /bin/sleep 0.3; exit 3 }}"
        )
    });
    assert_eq!(o.status, 3, "stderr: {}", o.stderr);
    assert!(ran, "the cleanup's command never finished");
    assert!(
        took < Duration::from_secs(3),
        "it waited out the grace: {took:?}"
    );
}

#[test]
fn a_torn_down_stages_cleanup_write_is_not_cut_at_an_edge_the_teardown_left() {
    // A ral write past what the pipe holds, to the edge of a `sleep` the
    // teardown kills: read, not dead, so the cleanup goes on.
    let (o, ran, took) = run_marked_timed("ral_pipe_teardown_cleanup_write", |f| {
        format!(
            "!{{ guard {{ () }} {{ |_| /bin/sleep 5 }} {{ |_ _| for !{{range 0 4000}} {{ |i| echo 0123456789012345678901234567890123456789 }}; touch {f} }} }} | /bin/sleep 5 | !{{ /bin/sleep 0.3; exit 3 }}"
        )
    });
    assert_eq!(o.status, 3, "stderr: {}", o.stderr);
    assert!(ran, "the cleanup was cut or blocked at its write");
    assert!(
        took < Duration::from_secs(3),
        "it waited out the grace: {took:?}"
    );
}

#[test]
fn a_rolled_back_stages_cleanup_does_not_block_on_the_edge_its_reader_never_took() {
    let markers = tempfile::tempdir().expect("create rollback markers directory");
    let acquired = markers.path().join("acquired");
    let released = markers.path().join("released");
    let script = format!(
        "!{{ guard {{ touch '{}' }} {{ |_| /bin/sleep 5 }} {{ |_ _| {PAST_ANY_PIPE}; touch '{}' }} }} | ral-no-such-command",
        acquired.display(),
        released.display(),
    );
    let started = std::time::Instant::now();
    let o =
        run_with_timeout(&[], &script, Duration::from_secs(15)).expect("rollback pipeline hung");
    let took = started.elapsed();

    assert_eq!(o.status, 127, "stderr: {}", o.stderr);
    // Lookup failure may stop the stage before acquisition; neither schedule is forced.
    assert_eq!(
        acquired.exists(),
        released.exists(),
        "release must finish iff acquisition ran; stderr: {}",
        o.stderr
    );
    assert!(
        took < Duration::from_secs(3),
        "it waited out the grace: {took:?}"
    );
}

#[test]
fn an_exit_in_a_stage_outranks_a_sibling_error() {
    let o = run_with_timeout(&[], "!{ exit 3 } | !{ fail x }", Duration::from_secs(10))
        .expect("exit-vs-error pipeline hung");
    assert_eq!(o.status, 3, "stderr: {}", o.stderr);
}

#[test]
fn an_exit_in_a_stage_ends_the_pipeline_at_once() {
    let o = run_with_timeout(
        &[],
        "!{ exit 3 } | !{ /bin/sleep 30 }",
        Duration::from_secs(3),
    )
    .expect("the sibling's sleep held the pipeline open past the exit");
    assert_eq!(o.status, 3, "stderr: {}", o.stderr);
}

#[test]
fn a_reader_gone_stages_cleanup_exit_is_the_verdict() {
    let o = run_with_timeout(
        &[],
        "!{ guard { () } { |_| yes } { |_ _| exit 3 } } | head -1",
        Duration::from_secs(10),
    )
    .expect("cleanup-exit pipeline hung");
    assert_eq!(o.status, 3, "stderr: {}", o.stderr);
}

#[test]
fn a_nested_reader_gone_stages_sibling_error_is_the_verdict() {
    // The inner final stage's reader-gone stop names the outer stage and
    // ranks under the failure beside it.
    let o = run_with_timeout(
        &[],
        "!{ !{ fail x } | !{ yes } } | head -1",
        Duration::from_secs(10),
    )
    .expect("nested pipeline hung");
    assert_eq!(o.status, 1, "stderr: {}", o.stderr);
}
