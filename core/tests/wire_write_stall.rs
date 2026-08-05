//! A peer that stops reading must never be able to wedge the front-end
//! behind the one `Mutex<WireChannel>` every write serialises on. Before
//! `WireChannel::set_write_deadline` existed, a stuck write parked that lock
//! forever: `Drop`, a cancel, and the heartbeat's own `Ping` all queued
//! behind it, and if the ticker's `Ping` was the stuck write, death was never
//! declared at all.
//!
//! Every test here drives a real `UnixStream::pair()`, one end handed to
//! `WireTransport::adopt` and the other held by the test as the "peer" whose
//! behaviour — silent, flooding, or eventually draining — the test controls
//! directly. A brisk [`Liveness`] keeps every bound well under a second, so
//! nothing here waits out the 25s production default.

#![cfg(unix)]

use ral_core::transport::{Control, DispatchId, Liveness, Program, Run, Transport, WireTransport};
use ral_core::types::Capabilities;
use ral_core::wire::WireChannel;
use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    Run {
        program: Program::Source("x".repeat(16 * 1024 * 1024)),
        script_name: "<test>".into(),
        caps: Capabilities::root(),
        wall: None,
        deferred_lease: None,
        worker_cap: None,
        io: RunIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: RunStdin::Empty,
        trail: None,
    }
}

/// A generous bound against dev-fleet jitter, still an order of magnitude
/// under the production 25s default: every assertion here must resolve well
/// inside the brisk write deadline above, not merely inside this ceiling.
const BOUND: Duration = Duration::from_secs(5);

/// A stuck write cannot mute death: the peer never reads a byte, so the
/// dispatch's write parks holding the write lock, and once the socket's own
/// write deadline elapses the connection is severed — closing the event
/// stream and setting `dead()`, exactly as a read EOF would.
#[test]
fn a_stuck_write_still_declares_death() {
    let (host_end, peer_end) = UnixStream::pair().expect("socketpair");
    let transport = Arc::new(WireTransport::adopt(host_end, brisk_liveness()).expect("adopt"));
    // Held open and never read: the peer that goes quiet mid-connection.
    let _peer = peer_end;

    let dispatcher = transport.clone();
    std::thread::spawn(move || {
        dispatcher.dispatch(DispatchId(1), big_run());
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
        transport.dead(),
        "a severed write-stalled connection must be reported dead"
    );
}

/// A write-stalled cancel returns and declares death: while the dispatch's
/// write parks holding the lock, `ControlSender::send` queues behind it
/// rather than deadlocking, and returns once the stalled write times out and
/// severs the connection — a cancel is never silently lost to a peer that
/// stopped reading.
#[test]
fn a_write_stalled_cancel_returns_and_declares_death() {
    let (host_end, peer_end) = UnixStream::pair().expect("socketpair");
    let transport = Arc::new(WireTransport::adopt(host_end, brisk_liveness()).expect("adopt"));
    let _peer = peer_end;

    let dispatcher = transport.clone();
    std::thread::spawn(move || {
        dispatcher.dispatch(DispatchId(1), big_run());
    });
    // Give the dispatch a moment to park mid-write, holding the write lock.
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    transport.control().send(Control::Cancel(DispatchId(1)));
    let elapsed = started.elapsed();

    assert!(
        elapsed < BOUND,
        "a cancel behind a stalled write must return once that write times out, took {elapsed:?}"
    );
    assert!(
        transport.dead(),
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

    let mut peer_ch = WireChannel::from_stream(peer_end);
    std::thread::spawn(move || {
        let mut seq: u64 = 0;
        loop {
            seq += 1;
            if peer_ch
                .write_frame(&ral_core::transport::Frame::Pong(seq))
                .is_err()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let dispatcher = transport.clone();
    std::thread::spawn(move || {
        dispatcher.dispatch(DispatchId(1), big_run());
    });

    let deadline = Instant::now() + BOUND;
    while !transport.dead() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        transport.dead(),
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
        dispatcher.dispatch(DispatchId(1), big_run());
    });

    let deadline = Instant::now() + BOUND;
    while !transport.dead() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        transport.dead(),
        "setup: the stalled write must declare death"
    );

    // Only now does the peer resume reading: whatever crossed before the
    // sever, and nothing legitimate after it.
    let mut peer_ch = WireChannel::from_stream(peer_end);
    let mut well_formed = 0;
    while let Ok(Some(_)) = peer_ch.read_frame() {
        well_formed += 1;
    }
    assert_eq!(
        well_formed, 0,
        "a write severed mid-frame must leave the peer with EOF or a decode error, never a complete frame"
    );
}
