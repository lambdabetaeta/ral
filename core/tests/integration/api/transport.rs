//! The transport under a front-end's hands.
//!
//! A peer that stops reading must never be able to wedge the front-end
//! behind the one `Mutex<WireChannel>` every write serialises on. Before
//! `WireChannel::set_write_deadline` existed, a stuck write parked that lock
//! forever: `Drop`, a cancel, and the heartbeat's own `Ping` all queued
//! behind it, and if the ticker's `Ping` was the stuck write, death was never
//! declared at all.
//!
//! Every wire test drives a real `UnixStream::pair()`, one end handed to
//! `WireTransport::adopt` and the other held by the test as the "peer" whose
//! behaviour — silent, flooding, or eventually draining — the test controls
//! directly. A brisk [`Liveness`] keeps every bound well under a second, so
//! nothing waits out the 25s production default.
//!
//! The control sender is driven the way a front-end drives it: an interrupt
//! arriving mid-dispatch must actually stop the run.  `ControlSender::interrupt`
//! aims at the dispatch this front-end has in flight, and the identity arm
//! strikes the dispatch node that `dispatch` opened for it, beneath which
//! `run_under` seats the run's frame. The wall clock is the discriminating
//! half: the child would sleep far longer than the ceiling asserted here.  A
//! SIGINT reaches the engine the same way, forwarded by its host as
//! `Control::Strike(_, Interrupted)`.

#![cfg(unix)]

use ral_core::HostSurface;
use ral_core::carrier::{Host, IdentityTransport, Transport, WireTransport, dispatch_to_report};
use ral_core::data::datum::Datum as _;
use ral_core::engine::EngineInstaller;
use ral_core::fact::ExitCode as _;
use ral_core::io::terminal::TerminalState;
use ral_core::path::EnginePath;
use ral_core::protocol::channel::Liveness;
use ral_core::protocol::{Attach, DispatchId, Ran, Report, Run};
use ral_core::shell::runtime::{CapturePolicy, Observed};
use ral_core::source::SourceOrigin;
use ral_core::test_helper::eventually;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The wire transport ignores the host it is handed on `dispatch` — see
/// `Transport::dispatch`'s doc — so the mute host is all any test here needs.
fn no_seam() -> Arc<dyn Host> {
    Arc::new(())
}

/// Interval is irrelevant to every test here — none waits on the ticker's own
/// silence check — but the deadline is the one bound the whole suite proves
/// against: how long a stalled write may block before the write itself is
/// read as the peer's death.
fn brisk_liveness() -> Liveness {
    Liveness {
        interval: Duration::from_millis(30),
        deadline: Duration::from_millis(300),
    }
}

/// A dispatch large enough to overflow any plausible unix-domain socket
/// buffer many times over, so a peer that never drains it reliably parks the
/// write rather than merely slowing it down.
fn big_run() -> Run {
    Run::captured("x".repeat(16 * 1024 * 1024), SourceOrigin::named("<test>"))
}

/// Whether `peer` has bytes waiting, without consuming any: the one
/// observable proof that the other end has begun a write. A write holds the
/// transport's write lock for its whole frame, so against a peer that never
/// drains, bytes on the wire mean the writer is parked holding that lock.
fn has_inbound_bytes(peer: &UnixStream) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let mut fds = [PollFd::new(peer, PollFlags::IN)];
    poll(&mut fds, Some(&now)).expect("poll") > 0
}

/// A generous bound against dev-fleet jitter, still an order of magnitude
/// under the production 25s default: every assertion here must resolve well
/// inside the brisk write deadline above, not merely inside this ceiling.
const BOUND: Duration = Duration::from_secs(5);

/// A stuck write cannot mute death: the peer never reads a byte, so the
/// dispatch's write parks holding the write lock, and once the socket's own
/// write deadline elapses the connection is severed — closing the event
/// stream and setting `severed()`, exactly as a read EOF would.
#[test]
fn a_stuck_write_still_declares_death() {
    let (host_end, peer_end) = UnixStream::pair().expect("socketpair");
    let transport = Arc::new(WireTransport::adopt(host_end, brisk_liveness()).expect("adopt"));
    // Held open and never read: the peer that goes quiet mid-connection.
    let _peer = peer_end;

    let dispatcher = transport.clone();
    std::thread::spawn(move || {
        dispatcher.dispatch(DispatchId(1), big_run(), &no_seam(), None);
    });

    let started = Instant::now();
    let got = transport.events().recv();
    let elapsed = started.elapsed();

    assert!(
        got.is_none(),
        "a peer that never reads must close the event stream, not deliver an event"
    );
    assert!(
        elapsed < BOUND,
        "recv() must unblock once the stalled write is severed, took {elapsed:?}"
    );
    assert!(
        transport.severed().is_some(),
        "a severed write-stalled connection must be reported dead"
    );
}

/// A write-stalled cancel returns and declares death: while the dispatch's
/// write parks holding the lock, `ControlSender::cancel` queues behind it
/// rather than deadlocking, and returns once the stalled write times out and
/// severs the connection — a cancel is never silently lost to a peer that
/// stopped reading.
#[test]
fn a_write_stalled_cancel_returns_and_declares_death() {
    let (host_end, peer_end) = UnixStream::pair().expect("socketpair");
    let transport = Arc::new(WireTransport::adopt(host_end, brisk_liveness()).expect("adopt"));
    // Held open and never read: the peer that goes quiet mid-connection.
    let peer = peer_end;

    let dispatcher = transport.clone();
    std::thread::spawn(move || {
        dispatcher.dispatch(DispatchId(1), big_run(), &no_seam(), None);
    });
    // Park the cancel behind the dispatch, not ahead of it: a cancel that won
    // the lock first would write its frame into an empty buffer and return.
    assert!(
        eventually(BOUND, || has_inbound_bytes(&peer).then_some(())).is_some(),
        "setup: the dispatch must be mid-write, holding the write lock"
    );

    let started = Instant::now();
    transport.control().cancel(DispatchId(1));
    let elapsed = started.elapsed();

    assert!(
        elapsed < BOUND,
        "a cancel behind a stalled write must return once that write times out, took {elapsed:?}"
    );
    assert!(
        transport.severed().is_some(),
        "the stalled write must have declared death by the time the cancel returns"
    );
}

/// Liveness refresh cannot mask a write stall: the peer never reads a single
/// byte the front-end sends, but keeps flooding its own outbound frames —
/// exactly the traffic that would refresh `last_seen` and keep a
/// silence-only check from ever firing. The write-stall bound is
/// independent of that read-side traffic, so death is still declared.
#[test]
fn liveness_refresh_cannot_mask_a_write_stall() {
    let (host_end, peer_end) = UnixStream::pair().expect("socketpair");
    let transport = Arc::new(WireTransport::adopt(host_end, brisk_liveness()).expect("adopt"));

    let mut peer_ch = ral_core::test_access::wire_from_stream(peer_end);
    std::thread::spawn(move || {
        let mut seq: u64 = 0;
        loop {
            seq += 1;
            if ral_core::test_access::wire_write_frame(
                &mut peer_ch,
                &ral_core::protocol::Frame::Pong(seq),
            )
            .is_err()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let dispatcher = transport.clone();
    std::thread::spawn(move || {
        dispatcher.dispatch(DispatchId(1), big_run(), &no_seam(), None);
    });

    assert!(
        eventually(BOUND, || transport.severed()).is_some(),
        "a stalled write must still declare death despite continuous read-side traffic"
    );
}

/// Severance after a write error: once the stalled dispatch's write times out
/// and severs the connection, the peer resumes reading and drains whatever
/// made it onto the wire. That stream must end in EOF or a decode error and
/// never yield a further well-formed frame — a fresh frame appended after the
/// truncation would mean the hole a timed-out write left was papered over.
#[test]
fn no_well_formed_frame_follows_a_severed_write() {
    let (host_end, peer_end) = UnixStream::pair().expect("socketpair");
    let transport = Arc::new(WireTransport::adopt(host_end, brisk_liveness()).expect("adopt"));

    let dispatcher = transport.clone();
    std::thread::spawn(move || {
        dispatcher.dispatch(DispatchId(1), big_run(), &no_seam(), None);
    });

    assert!(
        eventually(BOUND, || transport.severed()).is_some(),
        "setup: the stalled write must declare death"
    );

    // Only now does the peer resume reading: whatever crossed before the
    // sever, and nothing legitimate after it.
    let mut peer_ch = ral_core::test_access::wire_from_stream(peer_end);
    let mut well_formed = 0;
    while let Ok(Some(_)) = ral_core::test_access::wire_read_frame(&mut peer_ch) {
        well_formed += 1;
    }
    assert_eq!(
        well_formed, 0,
        "a write severed mid-frame must leave the peer with EOF or a decode error, never a complete frame"
    );
}

static BARE: [EngineInstaller; 1] = [EngineInstaller {
    tag: "bare",
    runtime: |_| HostSurface::default().runtime(TerminalState::default()),
    root: |_, _| Ok(()),
    narrow: |_, _| {
        Err(ral_core::error::Error::new(ral_core::fact::Reason::State(
            Box::new(ral_core::fact::StateFailure::NoChildEngines),
        )))
    },
}];

#[test]
fn an_interrupt_through_the_control_sender_stops_an_in_flight_run() {
    let temp = std::env::temp_dir();
    let transport = IdentityTransport::boot(
        &BARE,
        &Attach::new(
            "bare",
            EnginePath::Host(temp.clone()),
            EnginePath::Host(temp),
        ),
    )
    .expect("a bare engine boots");

    // The child marks itself started: an interrupt before dispatch is dropped.
    let dir = tempfile::tempdir().expect("a scratch directory");
    let marker = dir.path().join("started");
    let sender = transport.control().clone();
    let started_marker = marker.clone();
    std::thread::spawn(move || {
        while !started_marker.exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.interrupt();
    });

    let started = Instant::now();
    let report = dispatch_to_report(
        &transport,
        Run {
            trail: Some(CapturePolicy::Off),
            ..Run::captured(
                format!(
                    "/bin/sh -c ': >\"$0\"; exec /bin/sleep 30' '{}'",
                    marker.display()
                ),
                SourceOrigin::named("<test>"),
            )
        },
        Arc::new(()),
        None,
    )
    .expect("the identity transport sends the Report synchronously");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "the cancel never reached the run: it slept {elapsed:?} of its 30 s"
    );
    let Report::Ran(Ran { ending, trail, .. }) = report else {
        panic!("the run must reach evaluation, got {report:?}");
    };
    // The child dies of SIGINT, `Interrupt`'s own grace signal, so the run
    // reports the interrupt.
    assert_eq!(
        ending.code(),
        130,
        "an interrupted child is torn down with SIGINT"
    );

    // The struck `/bin/sleep` settles into an observation as the stop
    // unwinds, and `Audit::close` reads the trail however the run ended.
    let struck = trail.into_iter().find_map(|fo| {
        let obs = ral_core::shell::runtime::Observation::decode(&fo).ok()?;
        match obs.what {
            Observed::Command(c) if c.argv.iter().any(|a| a.contains("sleep")) => Some(c.argv),
            _ => None,
        }
    });
    assert!(
        struck.is_some(),
        "the Report must carry the struck command in its trail"
    );
}
