//! These drive `WireTransport::adopt` over a `UnixStream::pair`, a peer
//! `WireChannel` on the far end standing in for the guest engine, to prove the
//! two halves of the liveness law: any received frame is proof of life, and only
//! genuine silence past the deadline is death. Margins are deliberately loose,
//! since the dev fleet includes a jittery VM — nothing here asserts a tight
//! upper bound, only that death is or is not reached within seconds of slack.
#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
use super::*;
use crate::protocol::{Ending, PROTOCOL_VERSION, Report};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Instant;

/// Several pings fall inside one deadline window, and both are far below
/// the slack the assertions wait.
fn brisk() -> Liveness {
    Liveness {
        interval: Duration::from_millis(25),
        deadline: Duration::from_millis(250),
    }
}

/// The ticker does not start until the attach is written, so any test
/// expecting heartbeat traffic or deadline death must call this first.
fn attach(transport: &WireTransport) {
    transport.attach(Attach::new("repl", PathBuf::from("/"), PathBuf::from("/")));
}

/// The version handshake the guest engine checks before it will speak.
#[test]
fn adopt_then_attach_conveys_the_protocol_version() {
    let (front, back) = UnixStream::pair().unwrap();
    let mut peer = WireChannel::from_stream(back);

    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    attach(&transport);

    match peer
        .read_frame()
        .unwrap()
        .expect("an Attach frame must arrive")
    {
        Frame::Attach(attach) => assert_eq!(attach.proto_version, PROTOCOL_VERSION),
        other => panic!("expected an Attach frame, got {other:?}"),
    }
}

/// The attach is delayed past several ping intervals, so `Attach` arriving
/// first proves the ordering is by construction, not by winning a race.
#[test]
fn no_ping_precedes_the_attach_handshake() {
    let (front, back) = UnixStream::pair().unwrap();
    let mut peer = WireChannel::from_stream(back);

    let transport = WireTransport::adopt(front, brisk()).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    attach(&transport);

    match peer.read_frame().unwrap().expect("a frame must arrive") {
        Frame::Attach(_) => {}
        other => panic!("the first frame on the wire must be Attach, got {other:?}"),
    }
}

/// The law's positive half: a ponging peer is alive well past the
/// deadline, since received frames are proof of life.
#[test]
fn a_ponging_peer_keeps_the_session_alive_past_the_deadline() {
    let (front, back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, brisk()).unwrap();
    attach(&transport);

    let peer = std::thread::spawn(move || {
        let mut ch = WireChannel::from_stream(back);
        loop {
            match ch.read_frame() {
                Ok(Some(Frame::Ping(n))) => {
                    if ch.write_frame(&Frame::Pong(n)).is_err() {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });

    std::thread::sleep(Duration::from_secs(2));
    assert!(
        transport.severed().is_none(),
        "a ponging peer must keep the session alive well past the deadline"
    );

    drop(transport);
    let _ = peer.join();
}

/// The law's negative half. A closed event stream is also the mechanism
/// that fails an in-flight dispatch as cancelled.
#[test]
fn a_silent_peer_is_declared_dead_and_closes_the_event_stream() {
    let (front, back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, brisk()).unwrap();
    attach(&transport);

    // `back` is held open but never speaks: no EOF, so death can come from
    // nothing but the heartbeat deadline.
    assert!(
        transport.events().recv().is_none(),
        "a silent peer past the deadline must close the event stream"
    );
    assert!(
        transport.severed().is_some(),
        "silence past the deadline is death"
    );

    drop(back);
}

/// Silence is counted in probes, not in clock, so the peer is condemned
/// only after it has ignored a full deadline's worth of `Ping`s this
/// process actually sent. A host asleep sends none and so accuses nobody —
/// the sleep/wake false death this replaced.
#[test]
fn a_peer_is_condemned_by_unanswered_probes_not_by_elapsed_time() {
    let (front, back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, brisk()).unwrap();
    attach(&transport);

    // The peer reads but never answers, so every ping stays unanswered.
    let mut peer = WireChannel::from_stream(back);
    let mut pings = 0;
    while transport.severed().is_none() {
        match peer.read_frame() {
            Ok(Some(Frame::Ping(_))) => pings += 1,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(
        pings >= brisk().probes(),
        "death must follow a deadline's worth of probes, got {pings}"
    );
}

/// The reader severs before dropping its sender, so `severed()` is honest
/// the instant `recv` unblocks on a hangup.
#[test]
fn a_hangup_closes_the_event_stream_and_marks_death() {
    let (front, back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();

    drop(back);

    assert!(
        transport.events().recv().is_none(),
        "a hangup must close the event stream"
    );
    assert!(
        transport.severed().is_some(),
        "a hangup is death under every teardown path"
    );
}

/// The control path holds the same law as the data path: a `Cancel` the
/// wire will not take is death now, not once the reader gets round to an
/// EOF.
#[test]
fn a_failed_control_write_marks_the_transport_dead() {
    let (front, back) = UnixStream::pair().unwrap();
    // Only the front's write half is shut, so a write fails while `back`,
    // held open and unattached, leaves the reader nothing to read and the
    // ticker unspawned: the control write is the sole possible cause.
    front.shutdown(std::net::Shutdown::Write).unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();

    transport.control().cancel(DispatchId(1));

    assert!(
        transport.severed().is_some(),
        "a cancel that cannot be written is death, not silence"
    );
    drop(back);
}

/// A batch settling with no dispatch in flight — the shape a detached
/// worker's completion takes between runs — reaches a sink installed
/// through `set_deferred_sink`, and a dispatch that follows still
/// round-trips to its own Report: `Frame::Session` routing on the reader
/// does not disturb `Frame::Event`.
#[test]
fn session_frame_with_no_dispatch_reaches_the_installed_sink() {
    use crate::types::DeferredSink;

    struct RecordingSink {
        batches: Mutex<Vec<Vec<FOValue>>>,
    }
    impl DeferredSink for RecordingSink {
        fn deliver(&self, batch: Vec<FOValue>) {
            self.batches.lock().unwrap().push(batch);
        }
    }

    let (front, back) = UnixStream::pair().unwrap();
    let mut peer = WireChannel::from_stream(back);

    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    attach(&transport);
    match peer
        .read_frame()
        .unwrap()
        .expect("the Attach handshake must cross first")
    {
        Frame::Attach(_) => {}
        other => panic!("expected the Attach frame, got {other:?}"),
    }

    let sink = Arc::new(RecordingSink {
        batches: Mutex::new(Vec::new()),
    });
    transport.set_deferred_sink(sink.clone() as Arc<dyn DeferredSink>);

    peer.write_frame(&Frame::Session(SessionEvent::DeferredSurface(vec![
        FOValue::Int { value: 7 },
    ])))
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !sink.batches.lock().unwrap().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session batch never reached the installed sink"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        sink.batches.lock().unwrap()[0],
        vec![FOValue::Int { value: 7 }]
    );

    let id = DispatchId(1);
    transport.dispatch(
        id,
        Run::captured("$[1 + 1]", "<test>"),
        &(Arc::new(()) as Arc<dyn Host>),
    );

    match peer
        .read_frame()
        .unwrap()
        .expect("the dispatch must cross the wire")
    {
        Frame::Dispatch(did, _) => assert_eq!(did, id),
        other => panic!("expected a Dispatch frame, got {other:?}"),
    }
    peer.write_frame(&Frame::Event(
        id,
        Event::Report(Report::Ran {
            ending: Ending::Settled {
                value: FOValue::Int { value: 2 },
                status: 0,
            },
            captured: None,
            trail: Vec::new(),
        }),
    ))
    .unwrap();

    match transport.events().recv() {
        Some((rid, Event::Report(_))) => {
            assert_eq!(rid, id, "the dispatch still round-trips to its own Report");
        }
        other => panic!("expected the dispatch's Report, got {other:?}"),
    }
}
