// Integration tests for `watch`-spawned handles.
//
// `watch { ... }` spawns a handle whose stdout and stderr stream live
// through the shell multiplexer.  Each line is written to fd 1 prefixed by
// the handle's label (defaulting to `handle:N`).  `await` blocks until
// both streams have been drained, so output order relative to the caller
// is deterministic.
#![cfg(unix)]

mod common;

use common::run;

// Lines from a watched block appear on stdout prefixed by the handle label.
#[test]
fn watch_emits_prefixed_stdout_lines() {
    let out = run("ral_spawn_watch", r#"
        let h = watch "job" { echo one; echo two }
        await $h
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("[job] one"),
        "missing prefixed 'one' line: {:?}",
        out.stdout,
    );
    assert!(
        out.stdout.contains("[job] two"),
        "missing prefixed 'two' line: {:?}",
        out.stdout,
    );
}

// `await` must block until all of the watched handle's output has been
// printed; lines after the await must come strictly after the child's lines.
#[test]
fn watchawait_flushes_before_returning() {
    let out = run("ral_spawn_watch", r#"
        let h = watch "job" { echo from-watch-1; echo from-watch-2 }
        await $h
        echo marker-after
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let w1 = out
        .stdout
        .find("[job] from-watch-1")
        .expect("from-watch-1 missing");
    let w2 = out
        .stdout
        .find("[job] from-watch-2")
        .expect("from-watch-2 missing");
    let after = out
        .stdout
        .find("marker-after")
        .expect("marker-after missing");
    assert!(w1 < w2, "watched lines out of order: {:?}", out.stdout);
    assert!(
        w2 < after,
        "marker-after must follow watched output: {:?}",
        out.stdout
    );
}

// Two watched handles interleave but each line remains atomic and carries
// its own label.
#[test]
fn watch_two_handles_interleave_with_atomic_lines() {
    let out = run("ral_spawn_watch", r#"
        let h1 = watch "a" { echo alpha }
        let h2 = watch "b" { echo beta }
        await $h1
        await $h2
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    let mut seen_alpha = false;
    let mut seen_beta = false;
    for line in out.stdout.lines() {
        if line == "[a] alpha" {
            seen_alpha = true;
        } else if line == "[b] beta" {
            seen_beta = true;
        }
    }
    assert!(seen_alpha, "alpha missing from {:?}", out.stdout);
    assert!(seen_beta, "beta missing from {:?}", out.stdout);
}

// Stderr inside a watched block is prefixed `[label:err]`.  Driven through
// sh so the child emits directly to fd 2 without ral's error-handling path.
#[test]
fn watch_stderr_is_prefixed_err() {
    let out = run("ral_spawn_watch", r#"
        let h = watch "job" { sh -c "echo out; echo err 1>&2" }
        await $h
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("[job] out"),
        "missing stdout prefix in: {:?}",
        out.stdout,
    );
    assert!(
        out.stdout.contains("[job:err] err"),
        "missing stderr prefix in: {:?}",
        out.stdout,
    );
}

// Labels are evaluated in the caller's scope and may interpolate.
#[test]
fn watch_label_interpolates_from_caller_scope() {
    let out = run("ral_spawn_watch", r#"
        let target = "prod"
        let h = watch "build-$target" { echo step }
        await $h
        "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("[build-prod] step"),
        "missing interpolated prefix in: {:?}",
        out.stdout,
    );
}
