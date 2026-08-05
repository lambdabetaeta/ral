//! The transport's control door, driven the way a front-end drives it: a
//! `Control::Cancel` arriving mid-dispatch must actually stop the run.
//!
//! The identity arm of `ControlSender::send` trips the `ForegroundScope` that
//! `dispatch` files under the cancelled id, and `run_under` seats the run's
//! frame beneath it.  The id is the hinge: the dispatch counter starts at one
//! and `dispatch` restores nought on the way out, so a cancel naming
//! `DispatchId(0)` — or any dispatch but the one in flight — finds nothing
//! filed and is dropped on the floor.  Hence the cancel here is minted by
//! `cancel_in_flight`, the call the REPL's Ctrl-C makes, rather than by hand.
//! The wall clock is the discriminating half: the child would sleep far longer
//! than the ceiling asserted here.
//!
//! The ambient interrupt watermark is the other, independent leg of the same
//! fold, driven by SIGINT and exercised by the `run` and `process::cancel`
//! suites; nothing here rests on the session being signal-facing.

#![cfg(unix)]

use ral_core::transport::{IdentityTransport, Program, Report, Run, Transport, dispatch_to_report};
use ral_core::types::{Capabilities, CapturePolicy, Observed, Shell};
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use std::time::{Duration, Instant};

#[test]
fn a_cancel_through_the_control_door_stops_an_in_flight_run() {
    let shell = Shell::new(ral_core::io::TerminalState::default());
    let transport = IdentityTransport::new(shell);

    let sender = transport.control().clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        sender.cancel_in_flight();
    });

    let started = Instant::now();
    let report = dispatch_to_report(
        &transport,
        Run {
            program: Program::Source("/bin/sleep 30".into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: Some(CapturePolicy::Off),
        },
        |_| {},
        |_| unreachable!("no desk is installed"),
    )
    .expect("the identity transport sends the Report synchronously");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "the cancel never reached the run: it slept {elapsed:?} of its 30 s"
    );
    let Report::Ran { ending, trail, .. } = report else {
        panic!("the run must reach evaluation, got {report:?}");
    };
    // `terminate_group` reserves SIGINT for `Interrupt` and opens with SIGTERM
    // for every other cause, so the child dies of signal 15 and the run reports
    // the death it actually died of.
    assert_eq!(
        ending.status(),
        143,
        "an explicitly cancelled child is torn down with SIGTERM"
    );

    // The cancel unwinds as a `Break::Error`, so the struck `/bin/sleep`
    // itself still settles into an observation before the unwind reaches the
    // run door — `Audit::close` reads that prefix regardless of how the run
    // ended.
    let struck = trail.into_iter().find_map(|fo| {
        let obs = ral_core::types::Observation::from_value(&ral_core::Value::from(fo))?;
        match obs.what {
            Observed::Command { argv, .. } if argv.first().is_some_and(|a| a.contains("sleep")) => {
                Some(argv)
            }
            _ => None,
        }
    });
    assert!(
        struck.is_some(),
        "the Report must carry the struck command in its trail"
    );
}
