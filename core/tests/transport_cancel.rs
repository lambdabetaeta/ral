//! The transport's control door, driven the way a front-end drives it: an
//! interrupt arriving mid-dispatch must actually stop the run.
//!
//! `ControlSender::interrupt` aims at the dispatch this front-end has in
//! flight, and the identity arm strikes the `ForegroundScope` that `dispatch`
//! opened for it, beneath which `run_under` seats the run's frame. The wall
//! clock is the discriminating half: the child would sleep far longer than
//! the ceiling asserted here.  A SIGINT reaches the engine the same way,
//! forwarded by its host as this very `Control::Interrupt`.

#![cfg(unix)]

use ral_core::HostSurface;
use ral_core::carrier::{IdentityTransport, Transport, dispatch_to_report};
use ral_core::engine::EngineInstaller;
use ral_core::first_order::datum::Datum as _;
use ral_core::protocol::{Attach, Report, Run};
use ral_core::terminal::TerminalState;
use ral_core::types::{CapturePolicy, Observed};
use std::sync::Arc;
use std::time::{Duration, Instant};

static BARE: [EngineInstaller; 1] = [EngineInstaller {
    tag: "bare",
    runtime: |_| Ok(HostSurface::default().runtime(TerminalState::default())),
    root: |_, _| Ok(()),
    narrow: |_, _| Err("this engine makes no children".into()),
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
            trail: Some(CapturePolicy::Off),
            ..Run::captured("/bin/sleep 30", "<test>")
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
    // The child dies of SIGINT, `Interrupt`'s own grace signal, so the run
    // reports the interrupt.
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
        let obs = ral_core::types::Observation::decode(&fo).ok()?;
        match obs.what {
            Observed::Command(c) if c.argv.first().is_some_and(|a| a.contains("sleep")) => {
                Some(c.argv)
            }
            _ => None,
        }
    });
    assert!(
        struck.is_some(),
        "the Report must carry the struck command in its trail"
    );
}
