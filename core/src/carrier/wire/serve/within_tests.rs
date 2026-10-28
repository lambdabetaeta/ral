use super::*;
use crate::carrier::{Transport as _, WireTransport, dispatch_to_report};
use crate::path::EnginePath;
use crate::process::DurableRoot;
use crate::process::cancel::REQUEST_SERIAL;
use crate::protocol::Ending;
use crate::protocol::channel::Liveness;
use crate::source::SourceOrigin;
use crate::test_helper::eventually;

const WAIT: Duration = Duration::from_secs(5);

// The relay records actual host frames before the engine can answer them.
fn with_engine<R>(run: impl FnOnce(&WireTransport) -> R) -> (R, Vec<Frame>) {
    let (front, relay_front) = crate::process::cloexec_socketpair().unwrap();
    let (relay_back, back) = crate::process::cloexec_socketpair().unwrap();
    let end = relay_back.try_clone().unwrap();
    let mut from_front = WireChannel::from_stream(relay_front);
    let mut to_front = from_front.try_clone().unwrap();
    let mut from_back = WireChannel::from_stream(relay_back);
    let mut to_back = from_back.try_clone().unwrap();
    let uplink = std::thread::spawn(move || {
        let mut frames = Vec::new();
        while let Ok(Some(frame)) = from_front.read_frame() {
            frames.push(frame.clone());
            if to_back.write_frame(&frame).is_err() {
                break;
            }
        }
        end.shutdown(std::net::Shutdown::Write).unwrap();
        frames
    });
    let downlink = std::thread::spawn(move || {
        while let Ok(Some(frame)) = from_back.read_frame() {
            // Hold reports across polls to expose duplicate strikes.
            if matches!(&frame, Frame::Event(_, Event::Report(_))) {
                std::thread::sleep(POLL_SLICE * 3);
            }
            if to_front.write_frame(&frame).is_err() {
                break;
            }
        }
        to_front.shutdown();
    });
    let engine = std::thread::spawn(move || {
        engine_session(
            WireChannel::from_stream(back),
            &crate::carrier::testkit::BARE,
            Patience::default(),
        )
    });
    let transport = WireTransport::adopt(front, Liveness::default()).unwrap();
    // The process's own cwd and HOME: an in-process engine must not move either.
    transport.attach(Attach::new(
        "bare",
        EnginePath::Host(std::env::current_dir().unwrap()),
        EnginePath::Host(
            std::env::var_os("HOME").map_or_else(|| "/".into(), std::path::PathBuf::from),
        ),
    ));
    transport.await_attached().unwrap();
    let result = run(&transport);
    drop(transport);
    let frames = uplink.join().unwrap();
    downlink.join().unwrap();
    assert_eq!(engine.join().unwrap(), 0);
    (result, frames)
}

fn dispatch(transport: &WireTransport, src: &str, within: Option<&Scope>) -> Report {
    dispatch_to_report(
        transport,
        Run::captured(src, SourceOrigin::named("<test>")),
        Arc::new(()),
        within,
    )
    .expect("the wire engine answers the dispatch")
}

#[test]
fn cancelling_within_strikes_its_wire_dispatch_once_and_not_the_next() {
    let _serial = REQUEST_SERIAL.lock();
    let dir = tempfile::tempdir().unwrap();
    let within = DurableRoot::default().child();
    let ((first, next), frames) = with_engine(|transport| {
        let first = std::thread::scope(|threads| {
            let running = threads.spawn(|| {
                dispatch(
                    transport,
                    &format!(
                        "sh -c 'touch {}; exec sleep 30'",
                        dir.path().join("running").display()
                    ),
                    Some(&within),
                )
            });
            assert!(
                eventually(WAIT, || dir.path().join("running").exists().then_some(())).is_some(),
                "the child must start before cancellation"
            );
            within.cancel(CancelCause::Cancelled);
            assert!(
                eventually(WAIT, || running.is_finished().then_some(())).is_some(),
                "cancelling within must settle the wire dispatch promptly"
            );
            running.join().unwrap()
        });
        (first, dispatch(transport, "$[1 + 1]", None))
    });
    assert!(
        matches!(
            first,
            Report::Ran {
                ending: Ending::Stopped {
                    cause: CancelCause::Cancelled,
                    ..
                },
                ..
            }
        ),
        "the first dispatch must report Cancelled: {first:?}"
    );
    assert!(
        matches!(
            next,
            Report::Ran {
                ending: Ending::Settled {
                    value: FOValue::Int { value: 2 },
                    status: 0
                },
                ..
            }
        ),
        "the following dispatch must be untouched: {next:?}"
    );
    let dispatches: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Dispatch(id, _) => Some(*id),
            _ => None,
        })
        .collect();
    let controls: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Control(control) => Some(*control),
            _ => None,
        })
        .collect();
    assert_eq!(dispatches.len(), 2);
    assert_eq!(
        controls,
        [Control::Strike(dispatches[0], CancelCause::Cancelled)]
    );
}

#[test]
fn dispatch_local_causes_preserve_the_wire_engine_and_their_cause() {
    let _serial = REQUEST_SERIAL.lock();
    for cause in [
        CancelCause::ReaderGone,
        CancelCause::Interrupted,
        CancelCause::Cancelled,
        CancelCause::TimedOut,
        CancelCause::Terminated,
        CancelCause::Aborted,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let within = DurableRoot::default().child();
        within.cancel(cause);
        let ((), frames) = with_engine(|transport| {
            let report = dispatch(
                transport,
                &format!("touch {}", dir.path().join("effect").display()),
                Some(&within),
            );
            assert!(
                matches!(report, Report::Ran {
                ending: Ending::Stopped { cause: actual, .. }, ..
            } if actual == cause),
                "the dispatch must preserve {cause:?}: {report:?}"
            );
            assert!(!dir.path().join("effect").exists());
            assert_eq!(transport.session_ended().unwrap(), None);
            assert!(matches!(
                dispatch(transport, "$[1 + 1]", None),
                Report::Ran {
                    ending: Ending::Settled {
                        value: FOValue::Int { value: 2 },
                        status: 0
                    },
                    ..
                }
            ));
        });
        assert!(frames.iter().any(|frame| matches!(frame,
            Frame::Control(Control::Strike(_, actual)) if *actual == cause
        )));
        assert!(
            !frames
                .iter()
                .any(|frame| matches!(frame, Frame::Control(Control::Terminate | Control::Abort)))
        );
    }
}

#[test]
fn a_wire_dispatch_under_cancelled_within_runs_no_effect() {
    let _serial = REQUEST_SERIAL.lock();
    let dir = tempfile::tempdir().unwrap();
    let within = DurableRoot::default().child();
    within.cancel(CancelCause::Cancelled);

    let (report, frames) = with_engine(|transport| {
        dispatch(
            transport,
            &format!("touch {}", dir.path().join("effect").display()),
            Some(&within),
        )
    });

    assert!(
        matches!(
            report,
            Report::Ran {
                ending: Ending::Stopped {
                    cause: CancelCause::Cancelled,
                    ..
                },
                ..
            }
        ),
        "the dispatch must be born Cancelled: {report:?}"
    );
    assert!(!dir.path().join("effect").exists());
    let frames: Vec<_> = frames
        .into_iter()
        .filter(|frame| matches!(frame, Frame::Dispatch(..) | Frame::Control(_)))
        .collect();
    assert!(
        matches!(frames.as_slice(), [Frame::Control(Control::Strike(struck, CancelCause::Cancelled)), Frame::Dispatch(opened, _)] if struck == opened),
        "the strike must precede its own dispatch: {frames:?}"
    );
}
