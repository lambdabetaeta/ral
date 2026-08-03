//! The transport's control door, driven the way a front-end drives it: a
//! `Control::Cancel` arriving mid-dispatch must actually stop the run.
//!
//! The identity arm of `ControlSender::send` raises nothing but the ambient
//! interrupt watermark, so the cancel reaches the run only because the host
//! declared this session signal-facing before wiring the transport — the
//! frame's root then hears shutdown and its foreground is judged against the
//! watermark.  Wire a transport over a deaf shell and Ctrl-C in the REPL
//! becomes a silent no-op.  The wall clock is the discriminating half: the
//! child would sleep far longer than the ceiling asserted here.
//!
//! Its own test binary, so the process-global watermark needs no serialising
//! lock.

#![cfg(unix)]

use ral_core::transport::{
    Control, DispatchId, IdentityTransport, Program, Report, Run, Transport, dispatch_to_report,
};
use ral_core::types::{Capabilities, Shell};
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use std::time::{Duration, Instant};

#[test]
fn a_cancel_through_the_control_door_stops_an_in_flight_run() {
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    shell.face_signals();
    let transport = IdentityTransport::new(shell);

    let sender = transport.control().clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        // The identity arm ignores the id: it raises the ambient watermark.
        sender.send(Control::Cancel(DispatchId(0)));
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
    let Report::Ran { status, .. } = report else {
        panic!("the run must reach evaluation, got {report:?}");
    };
    // `terminate_group` reserves SIGINT for `Interrupt` and opens with SIGTERM
    // for every other cause, so the child dies of signal 15 and the run reports
    // the death it actually died of.
    assert_eq!(
        status, 143,
        "an explicitly cancelled child is torn down with SIGTERM"
    );
}
