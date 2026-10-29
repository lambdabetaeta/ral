#![allow(clippy::disallowed_methods, reason = "test code")]
// The whole file is Unix-only: it stands a `socketpair` in for the vsock
// stream, and both the stand-in and the [`WireTransport`] under test exist
// only there.  An empty test binary on Windows is the honest outcome — the
// laws below are laws about a transport that platform does not have.
#![cfg(unix)]

//! §3 frame protocol over vsock — the heartbeat and run-durability laws
//! (`docs/ral-wiki/design/engine-protocol.md`), proven end to end against a *real*
//! engine child.
//!
//! A `socketpair` stands in for the vsock stream: the codec is
//! transport-agnostic by design, so the engine cannot tell whether fd 3 is
//! one end of a same-host `socketpair` or a connection to the host over
//! `AF_VSOCK`. The guest side of the stand-in mirrors ral-guest's spawn
//! contract (`ral-guest/src/engine.rs::spawn`): the engine is this multicall
//! binary re-exec'd with `--engine`, its protocol socket on fd 3. The test
//! binary serves `--engine` itself through the shared pre-`main` `#[ctor]`
//! ([`exarch::pre_main_ctor!`]), so `std::env::current_exe()` is a genuine
//! engine.

use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "../src/test_helper.rs"]
mod test_helper;

use ral_core::carrier::{Host, Transport, WireTransport, dispatch_to_report};
use ral_core::path::EnginePath;
use ral_core::protocol::channel::Liveness;
use ral_core::protocol::{Attach, DispatchId, EngineSpec, Event, Report, Root, Run};
use ral_core::source::SourceOrigin;

// Mirror the binary's pre-`main` re-exec dispatch: the `--engine` re-exec is
// served here, before libtest sees a flag it would reject, so a re-exec'd
// child becomes the engine rather than a second copy of the test harness.
// See [`exarch::pre_main_ctor!`].
exarch::pre_main_ctor!();

/// Kills and reaps the engine child on drop, so no engine — and no `sleep`
/// it spawned — outlives a failed assertion.
struct EngineChild(std::process::Child);

impl Drop for EngineChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn engine_over_socketpair(liveness: Liveness) -> (WireTransport, EngineChild) {
    let (transport, child) = test_helper::engine_over_socketpair(liveness);
    (transport, EngineChild(child))
}

/// Attach the session to a fresh tempdir as cwd/home, tagging the engine's
/// builtin installer with exarch's own [`exarch::INSTALLERS`] tag.
fn attach(transport: &WireTransport) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let attach = Attach::root(
        EngineSpec::new(
            exarch::INSTALLERS[0].tag,
            EnginePath::Host(dir.path().to_path_buf()),
        ),
        Root::new(EnginePath::Host(dir.path().to_path_buf())).with_env(vec![(
            exarch::app::SCRATCH_VAR.to_owned(),
            dir.path().to_string_lossy().into_owned(),
        )]),
    );
    transport.attach(attach);
    transport
        .await_attached()
        .expect("the engine must accept the attach");
    dir
}

/// The §3 heartbeat keeps an *idle* session alive across a real child: adopt
/// → attach → a run settles to a `Report::Ran { Ok }`, and then the
/// session, left silent for wall-clock time well past its read deadline,
/// stays live — because the engine's `Pong` echoes of the host's `Ping`s are
/// the traffic that resets the deadline.
#[test]
fn an_idle_session_stays_alive_on_the_heartbeat_alone() {
    let (transport, _child) = engine_over_socketpair(Liveness {
        interval: Duration::from_millis(50),
        deadline: Duration::from_secs(2),
    });
    let _dir = attach(&transport);

    let report = dispatch_to_report(
        &transport,
        Run::captured("$[1 + 1]", SourceOrigin::named("<test>")),
        Arc::new(()),
        None,
    )
    .expect("the engine must answer the dispatch with a Report");

    assert!(
        matches!(
            report,
            Report::Ran {
                ending: ral_core::protocol::Ending::Settled { .. },
                ..
            }
        ),
        "`$[1 + 1]` must settle to Report::Ran {{ Ok }}, got {report:?}"
    );

    // Idle comfortably past the 2s deadline. Nothing but the heartbeat crosses
    // the protocol in this window; if the Pong traffic did not reset the deadline,
    // the engine would be declared dead here.
    std::thread::sleep(Duration::from_secs(3));

    assert!(
        transport.severed().is_none(),
        "an idle session must stay alive on the heartbeat past its deadline"
    );
}

/// A `Cancel` that overtakes the `Dispatch` it names still stops that run.
///
/// The scoped dispatcher may strike before sending. The engine must retain
/// that strike and spend it on the named run when its dispatch arrives.
#[test]
fn a_cancel_that_overtakes_its_dispatch_still_stops_the_run() {
    let (transport, _child) = engine_over_socketpair(Liveness::default());
    let _dir = attach(&transport);

    let id = DispatchId(7);
    transport.control().cancel(id);
    let started = Instant::now();
    transport.dispatch(
        id,
        Run::captured("/bin/sleep 30", SourceOrigin::named("<test>")),
        &(Arc::new(()) as Arc<dyn Host>),
        None,
    );

    let report = loop {
        let (did, event) = transport.events().recv().expect("the engine must answer");
        if did == id
            && let Event::Report(report) = event
        {
            break report;
        }
    };
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "the overtaking cancel was dropped: the run slept {elapsed:?} of its 30 s"
    );
    let Report::Ran { ending, .. } = report else {
        panic!("the run must reach evaluation, got {report:?}");
    };
    // The scope is cancelled before the run reads it, so `sleep` is never
    // spawned and the eval unwinds at its first check point.
    assert_eq!(
        ending.status(),
        143,
        "a run born under a cancelled scope unwinds without spawning"
    );
}

/// A dead peer fails the in-flight run as cancelled: a run that would take
/// far longer than the test (`sleep 30`) is dispatched and left running;
/// dropping the transport shuts the socket, so the engine sees EOF, cancels
/// the in-flight run, waits for it to settle, and exits — well within a
/// generous bound, rather than running the sleep to term or hanging on a
/// peer that will never speak again.
#[test]
fn a_dead_peer_fails_the_in_flight_run_as_cancelled() {
    let (transport, mut child) = engine_over_socketpair(Liveness::default());
    let _dir = attach(&transport);

    // Fire-and-forget: write the Dispatch frame and do not drain its Report.
    // A literal id suffices — nothing here correlates a reply.
    transport.dispatch(
        DispatchId(1),
        Run::captured("/bin/sleep 30", SourceOrigin::named("<test>")),
        &(Arc::new(()) as Arc<dyn Host>),
        None,
    );

    // Give the engine time to actually enter the sleep, so what the dropped
    // peer interrupts is a genuinely in-flight run, not a race with dispatch.
    std::thread::sleep(Duration::from_secs(1));

    // The engine sees EOF on fd 3 once the transport drops.
    drop(transport);

    // The engine must cancel the in-flight `sleep 30` and exit well within a
    // bound far shorter than the sleep itself. Poll `try_wait` rather than
    // block, so a hang fails the test by timeout instead of wedging it.
    let deadline = Instant::now() + Duration::from_secs(20);
    let exited = loop {
        match child.0.try_wait().expect("try_wait the engine child") {
            Some(_) => break true,
            None if Instant::now() >= deadline => break false,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    assert!(
        exited,
        "a dead peer must fail the in-flight run as cancelled and exit, not run `/bin/sleep 30` to \
         term or hang"
    );
}
