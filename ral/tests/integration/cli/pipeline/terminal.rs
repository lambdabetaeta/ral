//! Foreground handoff and Ctrl-C through a pty: the pipeline's group owns the
//! terminal, and the key pressed on it is the run's.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use super::{parse_tagged_pgid, probe};
use crate::common::Output;
use crate::common::pty::PtySession;
use std::time::Duration;

// ── Foreground-handoff regressions ──────────────────────────────────────────
//
// A foreground pipeline that owns a tty cannot admit direct external
// launch: `resolve::resolve_launch` forces such stages through the ral
// helper, and launch releases helper job frames only after `tcsetpgrp`
// has handed the pty to the pipeline pgid.  The tests below open a real
// pty so `tcgetpgrp` is meaningful.
//
// A [`probe`] writes both `pgid:<tag>` and `tcpgrp:<tag>` to stderr.
// These tests ask: when the stage starts running user code, is it in the
// pgid that owns the controlling terminal?

fn run_pty_repl_until(
    line: &str,
    timeout: Duration,
    done: impl Fn(&str) -> bool,
) -> Result<Output, String> {
    let mut session = PtySession::spawn()?;
    session
        .send_line(line)
        .map_err(|e| format!("writing to PTY: {e}"))?;
    let reached = session.wait_until(timeout, done);
    let mut out = session.finish();
    out.status = if reached { 0 } else { 124 };
    Ok(out)
}

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

#[test]
fn pty_external_stage_runs_to_completion() {
    // Sanity-check helper-gated foreground handoff under a real pty and
    // REPL: foreground handoff is an interactive-only policy, so this
    // drives ral through `-i --norc` rather than the batch script
    // runner.  The helper reports both its pgid and the pty foreground
    // pgid; after launch releases its job frame they must match.
    let script = format!("printf \"\" | {}", probe("post"));
    let o = run_pty_repl_until(&script, Duration::from_secs(8), |text| {
        parse_tagged_pgid(text, "post").is_some()
            && parse_tagged_tcpgrp(text, "post").is_some()
            && text.matches("❯").count() >= 2
    })
    .unwrap_or_else(|e| panic!("{e}"));
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

#[test]
fn ctrl_c_ends_an_all_ral_foreground_pipeline() {
    // The terminal belongs to the pipeline's group, whose only process is
    // the anchor: the tty's SIGINT reaches nothing that can die, so the
    // anchor must witness it and the collector cancel the stages.
    let mut session = PtySession::spawn().unwrap_or_else(|e| panic!("{e}"));
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

const CTRL_C: u8 = 0x03;
const CTRL_BACKSLASH: u8 = 0x1c;

/// Type `line` at a pty REPL, press `key` once the terminal is lent, and wait
/// for the prompt to come back.  The line's marker is `$[1234*2]`, computed
/// because the tty echoes what was typed: a standalone `2468` output line
/// means execution ran on past the key.
fn press_on_the_lent_terminal(line: &str, key: u8) -> (bool, PtySession) {
    let mut session = PtySession::spawn().unwrap_or_else(|e| panic!("{e}"));
    session.send_line(line).expect("send failed");
    session
        .wait_for_lent_terminal(Duration::from_secs(8))
        .expect("the command never took the terminal");
    session.send_byte(key).expect("the key could not be sent");
    let ended = session.wait_until(Duration::from_secs(8), |t| t.matches("❯").count() >= 2);
    (ended, session)
}

/// The key pressed on a lent terminal is the run's, so `attempt` cannot
/// swallow it and the line stops.
fn assert_the_key_stops_the_line(line: &str) {
    let (ended, session) = press_on_the_lent_terminal(line, CTRL_C);
    let out = session.finish();
    assert!(ended, "the prompt never came back; stderr: {}", out.stderr);
    assert!(
        !out.stderr.lines().any(|line| line.trim() == "2468"),
        "attempt swallowed the Ctrl-C and the line ran on; stderr: {}",
        out.stderr
    );
}

#[test]
fn ctrl_c_on_a_lent_command_stops_the_line_through_attempt() {
    assert_the_key_stops_the_line("attempt { sleep 30 }; echo $[1234*2]");
}

#[test]
fn ctrl_c_on_a_lent_pipeline_stops_the_line_through_attempt() {
    assert_the_key_stops_the_line("attempt { sleep 30 | cat }; echo $[1234*2]");
}

/// The verdict is the earlier stage's `exit 3`, yet the frame is struck.
#[test]
fn ctrl_c_stops_the_line_whatever_the_pipelines_verdict() {
    assert_the_key_stops_the_line("attempt { sh -c #'exit 3'# | sleep 30 }; echo $[1234*2]");
}

/// Every stage traps the key and exits 0: only the pipeline's own ear, heard
/// out after the anchor ends, knows it was pressed.
#[test]
fn ctrl_c_stops_the_line_though_every_stage_traps_it() {
    assert_the_key_stops_the_line(
        "attempt { sh -c #'trap \"exit 0\" INT; sleep 30'# | sh -c #'trap \"exit 0\" INT; cat'# }; echo $[1234*2]",
    );
}

/// Ctrl-\ on a lent terminal aborts the job and the session lives on.
#[test]
fn ctrl_backslash_on_a_lent_command_ends_the_job_not_the_shell() {
    let (ended, mut session) =
        press_on_the_lent_terminal("attempt { sleep 30 }; echo $[1234*2]", CTRL_BACKSLASH);
    let stopped = !session.text().lines().any(|line| line.trim() == "2468");
    session.send_line("echo $[2*3]").expect("send failed");
    let lives = session.wait_until(Duration::from_secs(8), |t| {
        t.lines().any(|line| line.trim() == "6")
    });
    let out = session.finish();
    assert!(ended, "the prompt never came back; stderr: {}", out.stderr);
    assert!(
        stopped,
        "attempt swallowed the Ctrl-\\ and the line ran on; stderr: {}",
        out.stderr
    );
    assert!(
        lives,
        "the shell did not outlive Ctrl-\\; stderr: {}",
        out.stderr
    );
}

/// A command that catches the key and carries on has handled it: the line
/// runs on.
#[test]
fn a_key_the_lent_command_handles_does_not_stop_the_line() {
    let (_, mut session) = press_on_the_lent_terminal(
        "sh -c #'trap \"echo caught$((6*7))\" INT; sleep 2; echo alive$((6*7))'#; echo $[1234*2]",
        CTRL_C,
    );
    let ran_on = session.wait_until(Duration::from_secs(8), |t| {
        t.contains("caught42")
            && t.contains("alive42")
            && t.lines().any(|line| line.trim() == "2468")
    });
    let out = session.finish();
    assert!(
        ran_on,
        "a handled key must leave the line running; stderr: {}",
        out.stderr
    );
}

/// Ctrl-C while a capture shares ral's own group reaches that group alone: a
/// worker spawned beforehand runs on to its end.
#[test]
fn ctrl_c_during_a_foreground_capture_spares_a_worker() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("worker-done");
    let mut session = PtySession::spawn().unwrap_or_else(|e| panic!("{e}"));
    session
        .send_line(&format!(
            "let h = spawn {{ sleep 2; touch #'{}'# }}; let x = !{{ sh -c #'echo held$((6*7)) >&2; sleep 5'# }}",
            marker.display()
        ))
        .expect("send failed");
    let held = session.wait_until(Duration::from_secs(8), |t| t.contains("held42"));
    assert!(held, "the capture never ran; stderr: {}", session.text());
    assert_eq!(
        session.foreground_pgid(),
        Some(session.pid()),
        "a capture must not be lent the terminal"
    );
    session.send_ctrl_c().expect("ctrl-c failed");
    let spared = session.poll_until(Duration::from_secs(5), |_| marker.exists());
    let out = session.finish();
    assert!(
        spared,
        "Ctrl-C reached the worker, which never finished; stderr: {}",
        out.stderr
    );
}

/// A worker's external leads a session of its own, with no controlling
/// terminal to open.
#[test]
fn a_workers_external_cannot_open_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let reach = dir.path().join("reach");
    let apart = dir.path().join("apart");
    let mut session = PtySession::spawn().unwrap_or_else(|e| panic!("{e}"));
    session
        .send_line(&format!(
            "spawn {{ sh -c #'true 3</dev/tty && touch \"{}\" || touch \"{}\"'# }}",
            reach.display(),
            apart.display()
        ))
        .expect("send failed");
    let settled = session.poll_until(Duration::from_secs(8), |_| reach.exists() || apart.exists());
    let out = session.finish();
    assert!(settled, "the worker never ran; stderr: {}", out.stderr);
    assert!(
        apart.exists() && !reach.exists(),
        "a worker's external opened the terminal; stderr: {}",
        out.stderr
    );
}
