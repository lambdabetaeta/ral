//! The transport's control door, driven the way a front-end drives it: an
//! interrupt arriving mid-dispatch must actually stop the run.
//!
//! The identity arm of `ControlSender` strikes the `ForegroundScope` that
//! `dispatch` opened for the dispatch in flight, and `run_under` seats the
//! run's frame beneath it — the interrupt the REPL's Ctrl-C sends, which names
//! no dispatch and so needs none minted by hand. The wall clock is the
//! discriminating half: the child would sleep far longer than the ceiling
//! asserted here.  A SIGINT reaches the engine the same way, forwarded by its
//! host as this very `Control::Interrupt`.

#![cfg(unix)]

use ral_core::engine::{Booted, EngineInstaller};
use ral_core::protocol::{
    Attach, IdentityTransport, Program, Report, Run, Transport, dispatch_to_report,
};
use ral_core::types::{CapturePolicy, GrantStack, Observed, Shell};
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[allow(
    clippy::unnecessary_wraps,
    reason = "must match EngineInstaller::boot's signature, which can genuinely refuse"
)]
fn bare(_attach: &Attach) -> Result<Booted, String> {
    Ok(Booted {
        shell: Shell::new(ral_core::io::TerminalState::default()),
        keep: Box::new(()),
    })
}

static BARE: [EngineInstaller; 1] = [EngineInstaller {
    tag: "bare",
    boot: bare,
    narrow: |_, _| Err("this engine hatches no children".into()),
}];

#[test]
fn an_interrupt_through_the_control_door_stops_an_in_flight_run() {
    let temp = std::env::temp_dir();
    let transport = IdentityTransport::boot(&BARE, &Attach::new("bare", temp.clone(), temp))
        .expect("a bare engine boots");

    let sender = transport.control().clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        sender.interrupt();
    });

    let started = Instant::now();
    let report = dispatch_to_report(
        &transport,
        Run {
            program: Program::Source("/bin/sleep 30".into()),
            script_name: "<test>".into(),
            caps: GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: Some(CapturePolicy::Off),
        },
        Arc::new(()),
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
    // `grace_signal` reserves SIGINT for `Interrupt`, so the child dies of
    // signal 2 and the run reports the death it actually died of.
    assert_eq!(
        ending.status(),
        130,
        "an interrupted child is torn down with SIGINT"
    );

    // The cancel unwinds as a `Break::Error`, so the struck `/bin/sleep`
    // itself still settles into an observation before the unwind reaches the
    // run door — `Audit::close` reads that prefix regardless of how the run
    // ended.
    let struck = trail.into_iter().find_map(|fo| {
        let obs = ral_core::types::Observation::from_wire(&fo)?;
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
