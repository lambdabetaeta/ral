//! These drive `WireTransport::adopt` over a `UnixStream::pair`, a peer
//! `WireChannel` on the far end standing in for the guest engine, to prove the
//! two halves of the liveness law: any received frame is proof of life, and only
//! genuine silence past the deadline is death. Margins are deliberately loose,
//! since the dev fleet includes a jittery VM — nothing here asserts a tight
//! upper bound, only that death is or is not reached within seconds of slack.
use super::*;
use crate::path::EnginePath;
use crate::process::CancelCause;
use crate::protocol::{Ending, PROTOCOL_VERSION, Ran, Report};
use crate::source::SourceOrigin;
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
    transport.attach(Attach::new(
        "repl",
        EnginePath::Host(PathBuf::from("/")),
        EnginePath::Host(PathBuf::from("/")),
    ));
}

#[test]
fn detach_requires_teardown_acknowledgment_not_eof() {
    for acknowledge in [false, true] {
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
        let peer = std::thread::spawn(move || {
            let mut ch = WireChannel::from_stream(back);
            assert!(matches!(ch.read_frame().unwrap(), Some(Frame::Attach(_))));
            ch.write_frame(&Frame::Session(SessionEvent::Attached))
                .unwrap();
            loop {
                match ch.read_frame().unwrap().unwrap() {
                    Frame::Ping(n) => ch.write_frame(&Frame::Pong(n)).unwrap(),
                    Frame::Detach => break,
                    other => panic!("expected Detach, got {other:?}"),
                }
            }
            if acknowledge {
                ch.write_frame(&Frame::Session(SessionEvent::Ended))
                    .unwrap();
            }
        });
        attach(&transport);
        transport.await_attached().unwrap();
        assert_eq!(transport.end().is_ok(), acknowledge);
        peer.join().unwrap();
    }
}

/// Neither a sender nor the ticker keeps the line: it goes within a tick of
/// the transport, and a stale sender's verbs are no-ops.
#[test]
fn a_dropped_transport_leaves_no_owner_of_its_line() {
    let (front, _peer) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, brisk()).unwrap();
    attach(&transport);
    let sender = transport.control().clone();
    let line = Arc::downgrade(&transport.line);
    drop(transport);

    let by = Instant::now() + Duration::from_secs(5);
    while line.upgrade().is_some() {
        assert!(
            Instant::now() < by,
            "the ticker must let go of the line within a tick"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    sender.end(crate::process::RootCause::Terminated);
    sender.cancel(DispatchId(1));
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

    std::thread::sleep(brisk().deadline * 3);
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
    assert_eq!(
        transport.severed(),
        Some(Severed::Closed(ConnectionClosed::Eof(ProtocolPeer::Engine)))
    );
}

/// The reader latches before it closes the stream, so the life bound to the
/// transport has ended, from the reader's thread, by the time `recv` fails.
#[test]
fn a_hangup_ends_the_bound_life() {
    let (front, back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    let life = crate::process::DurableRoot::default().child();
    transport.bind(life.clone());
    drop(back);
    assert!(transport.events().recv().is_none());
    assert_eq!(life.cause(), Some(CancelCause::Cancelled));
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
        matches!(
            transport.severed(),
            Some(Severed::Closed(ConnectionClosed::Io(ConnectionClosedIo {
                operation: ConnectionOperation::Write,
                ..
            })))
        ),
        "a cancel that cannot be written is a typed put failure"
    );
    drop(back);
}

/// A batch settling with no dispatch in flight — the shape a
/// worker's completion takes between runs — reaches a sink installed
/// through `set_deferred_sink`, and a dispatch that follows still
/// round-trips to its own Report: `Frame::Session` routing on the reader
/// does not disturb `Frame::Event`.
#[test]
fn session_frame_with_no_dispatch_reaches_the_installed_sink() {
    use crate::shell::runtime::DeferredSink;

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
        Run::captured("$[1 + 1]", SourceOrigin::named("<test>")),
        &(Arc::new(()) as Arc<dyn Host>),
        None,
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
        Event::Report(Report::Ran(Ran {
            ending: Ending::Ok(FOValue::Int { value: 2 }),
            rendered: String::new(),
            single_command: false,
            captured: None,
            trail: Vec::new(),
        })),
    ))
    .unwrap();

    match transport.events().recv() {
        Some((rid, Event::Report(_))) => {
            assert_eq!(rid, id, "the dispatch still round-trips to its own Report");
        }
        other => panic!("expected the dispatch's Report, got {other:?}"),
    }
}

/// An attach refusal with a hint, which must cross the wire with it.
fn refused() -> WireError {
    WireError::of(
        &Error::new(Reason::Protocol(Box::new(
            ProtocolFailure::VersionMismatch(crate::fact::ProtocolFailureVersionMismatch {
                front: PROTOCOL_VERSION + 1,
                engine: PROTOCOL_VERSION,
            }),
        )))
        .with_hint("ask the host"),
    )
}

#[test]
fn a_refusal_from_the_engine_is_the_attach_verdict() {
    let (front, back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    let peer = std::thread::spawn(move || {
        let mut ch = WireChannel::from_stream(back);
        assert!(matches!(ch.read_frame().unwrap(), Some(Frame::Attach(_))));
        ch.write_frame(&Frame::Session(SessionEvent::Refused(refused())))
            .unwrap();
    });
    attach(&transport);
    assert_eq!(transport.await_attached(), Err(Severed::Refused(refused())));
    peer.join().unwrap();
}

#[test]
fn every_attach_refusal_family_keeps_report_facts_and_discards_host_provenance() {
    use crate::carrier::testkit::{assert_normalized, host_error, reason_families};
    for reason in reason_families() {
        let original = host_error(reason);
        let wire = WireError::of(&original);
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
        let peer = std::thread::spawn(move || {
            let mut ch = WireChannel::from_stream(back);
            assert!(matches!(ch.read_frame().unwrap(), Some(Frame::Attach(_))));
            ch.write_frame(&Frame::Session(SessionEvent::Refused(wire)))
                .unwrap();
        });
        attach(&transport);
        let Err(Severed::Refused(received)) = transport.await_attached() else {
            panic!("an attach refusal is not closure or a report fault");
        };
        assert_normalized(&original, &received.to_error());
        peer.join().unwrap();
    }
}

#[test]
fn enquiry_answers_and_reports_preserve_all_families_but_not_host_provenance() {
    use crate::carrier::testkit::{assert_normalized, host_error, reason_families};
    use crate::fact::{ErrorFact, FailureCode as _};
    use crate::source::CallSite;
    struct Refuser(Error);
    impl Host for Refuser {
        fn surface(&self, _: &FOValue) {}
        fn enquire(&self, _: FOValue) -> Result<FOValue, Error> {
            Err(self.0.clone())
        }
    }
    for reason in reason_families() {
        let original = host_error(reason.clone());
        let expected = original.clone();
        let record = ErrorFact::new(
            reason,
            "engine-operation".into(),
            CallSite {
                script: "engine.ral".into(),
                line: 7,
                col: 3,
            },
        );
        let report_record = record.clone();
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
        // This peer supplies a report with its own site; host provenance must
        // not become that site's authority.
        let peer = std::thread::spawn(move || {
            let mut ch = WireChannel::from_stream(back);
            let Some(Frame::Dispatch(id, _)) = ch.read_frame().unwrap() else {
                panic!("dispatch")
            };
            ch.write_frame(&Frame::Event(
                id,
                Event::Enquiry(EnquiryId(17), FOValue::Unit),
            ))
            .unwrap();
            let Some(Frame::Answer(EnquiryId(17), Err(wire))) = ch.read_frame().unwrap() else {
                panic!("answer")
            };
            assert_normalized(&expected, &wire.to_error());
            ch.write_frame(&Frame::Event(
                id,
                Event::Report(Report::Ran(Ran {
                    ending: Ending::Err(report_record),
                    rendered: "engine report".into(),
                    single_command: false,
                    captured: None,
                    trail: vec![],
                })),
            ))
            .unwrap();
            // Keep the stream open until the transport's own drop, so a late
            // EOF cannot overtake the report under review.
            assert!(ch.read_frame().unwrap().is_none());
        });
        let report = crate::carrier::dispatch_to_report(
            &transport,
            Run::captured("ask", SourceOrigin::named("engine.ral")),
            Arc::new(Refuser(original.clone())),
            None,
        )
        .unwrap();
        let Report::Ran(Ran {
            ending: Ending::Err(back),
            ..
        }) = report
        else {
            panic!("err report")
        };
        assert_eq!(back, record);
        assert_eq!(back.reason(), &original.reason);
        assert_eq!(back.reason().failure_code(), original.failure_code());
        assert_eq!(back.frame(), "engine-operation");
        assert_eq!(back.site().script.as_ref(), "engine.ral");
        drop(transport);
        peer.join().unwrap();
    }
}

#[test]
fn unexpected_frame_keeps_receiver_phase_and_actual_kind() {
    for attached in [false, true] {
        let (front, back) = UnixStream::pair().unwrap();
        let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
        let mut peer = WireChannel::from_stream(back);
        if attached {
            peer.write_frame(&Frame::Session(SessionEvent::Attached))
                .unwrap();
        }
        peer.write_frame(&Frame::Ping(1)).unwrap();
        assert!(transport.events().recv().is_none());
        assert_eq!(
            transport.severed(),
            Some(Severed::Faulted(ConnectionFault::UnexpectedFrame(
                ConnectionFaultUnexpectedFrame {
                    receiver: ProtocolPeer::Frontend,
                    phase: if attached {
                        ProtocolPhase::Attached
                    } else {
                        ProtocolPhase::AwaitAttach
                    },
                    actual: crate::fact::FrameKind::Ping,
                },
            )))
        );
    }
}

#[test]
fn partial_frame_read_keeps_original_cause_separate_from_dump_evidence() {
    use std::io::Write as _;
    let (front, mut back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    back.write_all(&[1, 0]).unwrap();
    back.shutdown(std::net::Shutdown::Write).unwrap();
    assert!(transport.events().recv().is_none());
    assert_eq!(
        transport.severed(),
        Some(Severed::Closed(ConnectionClosed::Frame(
            crate::fact::FrameFailure::Partial(crate::fact::FrameFailurePartial {
                part: crate::fact::FramePart::Length,
                got: 2u64.into(),
                expected: 4u64.into(),
            }),
        )))
    );
}

#[test]
fn unencodable_attach_and_probe_keep_the_frame_encoding_cause() {
    use std::os::unix::ffi::OsStringExt as _;
    let invalid_path: PathBuf = std::ffi::OsString::from_vec(vec![b'/', 255]).into();
    let (front, _back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    let probe = Probe::PathBytes(invalid_path.clone());
    let expected = crate::io::frame::encode(&Frame::Probe(DispatchId(1), probe.clone()))
        .err()
        .unwrap();
    assert_eq!(
        transport.probe(&probe),
        Err(ProbeError::Rejected(ProbeFailure::Unencodable(expected)))
    );
    assert!(transport.severed().is_none());
    let attach = Attach::new(
        "test",
        EnginePath::Host(invalid_path),
        EnginePath::Host("/".into()),
    );
    let expected = crate::io::frame::encode(&Frame::Attach(attach.clone()))
        .err()
        .unwrap();
    transport.attach(attach);
    let Err(Severed::Refused(wire)) = transport.await_attached() else {
        panic!("encoding refusal")
    };
    assert_eq!(
        wire.reason,
        Reason::Protocol(Box::new(ProtocolFailure::Unencodable(
            ProtocolFailureUnencodable {
                operation: ProtocolOperation::Attach,
                cause: expected
            },
        )))
    );
}

#[test]
fn reader_panic_latches_reader_thread_ended_before_the_event_stream_closes() {
    struct PanickingSink;
    impl DeferredSink for PanickingSink {
        fn deliver(&self, _: Vec<FOValue>) {
            panic!("test sink panicked");
        }
    }
    let (front, back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    transport.set_deferred_sink(Arc::new(PanickingSink));
    let mut peer = WireChannel::from_stream(back);
    peer.write_frame(&Frame::Session(SessionEvent::DeferredSurface(vec![])))
        .unwrap();
    assert!(transport.events().recv().is_none());
    assert_eq!(
        transport.severed(),
        Some(Severed::Closed(ConnectionClosed::ReaderThreadEnded))
    );
}

#[test]
fn transport_drop_latches_typed_drop_even_before_the_late_reader_exits() {
    let (front, _back) = UnixStream::pair().unwrap();
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    let severance = transport.ends.severance.clone();
    drop(transport);
    assert_eq!(
        severance.cause(),
        Some(Severed::Closed(ConnectionClosed::TransportDropped))
    );
}
