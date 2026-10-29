use super::*;
use ral_core::shell::runtime::{BuiltinBody, BuiltinEntry};
use ral_core::ty::{Scheme, Ty};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
use std::borrow::Cow;
use std::sync::{Arc, Mutex, mpsc};

#[test]
fn headless_recording_failure_ends_the_run_in_both_formats() {
    for format in [OutputFormat::Text, OutputFormat::Json] {
        let root = frontend_root(Script::new(), Trunk::Returns);
        let recorder = root.recorder();
        let host = testkit::host(root);
        wait_ready(&host);
        recorder.fail_after(0);
        let why = run_frontend(&host, format).expect_err("a failed writer is not a clean finish");
        assert_eq!(why, recorder.check_available().unwrap_err().to_string());
        assert!(!host.roots()[0].is_live());
        assert!(host.roots()[0].steer("later".into()).is_err());
        assert_eq!(
            host.end(),
            [recorder.check_available().unwrap_err().to_string()],
            "recording loss travels through the terminal outcome"
        );
    }
}

#[test]
fn a_late_recording_fault_keeps_the_already_published_report() {
    for script in [reply_script("42"), no_reply_script()] {
        let root = frontend_root(script, Trunk::Returns);
        let recorder = root.recorder();
        let other = crate::agent::spawn::hatch_root(
            &root,
            crate::record::Memory::Amnemon,
            Some("independent".into()),
        )
        .unwrap();
        set_provider(
            &other,
            scripted("test-model", reply_script("'independent work completed'")),
        );
        let independent = other.handle();
        let host = Host::adopt(vec![(root, None), (other, None)]).unwrap();
        let root_id = host.roots()[0].id();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut headless = Headless::new(root_id, &mut out, &mut err);
        host.roots()[0].steer("finish first".into()).unwrap();
        let signals = receive_until(&host, |signal| {
            has_fact(signal, root_id, |record| {
                matches!(record, Record::Forensic(Forensic::Status { .. }))
            })
        });
        for signal in signals {
            headless.accept(signal);
        }
        let report = headless.status.clone();
        assert!(headless.finished());
        recorder.fail_after(0);
        let failure = recorder
            .emit(Display::Answer {
                text: "late display".into(),
            })
            .unwrap_err();
        let tail = receive_until(
            &host,
            |signal| matches!(signal, Signal::Transient(id, Transient::Died { .. }) if *id == root_id),
        );
        for signal in tail {
            headless.accept(signal);
        }
        assert_eq!(headless.status, report);
        assert_eq!(
            headless.fault.as_deref(),
            Some(failure.to_string().as_str())
        );
        assert!(recorder.check_available().is_err());
        assert!(host.roots()[0].steer("no next job".into()).is_err());
        assert!(
            independent.is_live(),
            "a writer fault ends only its owner's life"
        );
        independent.steer("continue independently".into()).unwrap();
        receive_until(&host, |signal| {
            has_fact(signal, independent.id(), |record| {
                matches!(record, Record::Forensic(Forensic::Status {
                    status: ChildStatus::Replied(FOValue::String { value }),
                }) if value == "independent work completed")
            })
        });
        assert_eq!(host.end(), [failure.to_string()]);
    }
}

fn unit_thunk(_unifier: &mut Unifier) -> Scheme {
    mk_scheme(&[], &[], thunk(pure(Ty::Unit)))
}

#[test]
fn headless_reply_is_not_clean_when_its_helpers_engine_teardown_fails() {
    let (entered, working) = mpsc::channel();
    let (release, held) = mpsc::channel();
    let held = Arc::new(Mutex::new(held));
    let installers = testkit::dressed(move |runtime| {
        let entries: Arc<[BuiltinEntry]> = vec![
            BuiltinEntry::new(
                Cow::Borrowed("headless-short-drain"),
                unit_thunk,
                "test-only: bound worker teardown wait.",
                BuiltinBody::Captured(Arc::new(|_, _, shell| {
                    shell.set_worker_drain_grace(Duration::from_millis(20));
                    Ok(ral_core::Value::Unit)
                })),
            ),
            BuiltinEntry::new(
                Cow::Borrowed("headless-held-worker"),
                unit_thunk,
                "test-only: ignore cancellation until released.",
                BuiltinBody::Captured(Arc::new(move |_, _, _| {
                    entered.send(()).unwrap();
                    held.lock().unwrap().recv_timeout(WAIT).unwrap();
                    Ok(ral_core::Value::Unit)
                })),
            ),
        ]
        .into();
        runtime.install_captured_builtins(&entries);
    });
    let mut root = Agent::for_test_with(crate::agent::TestTrunk {
        installers,
        ..crate::agent::TestTrunk::new("system")
    })
    .unwrap();
    set_provider(&root, scripted("test-model", reply_script("42")));
    let mut child = testkit::forked_child(&root, "helper", "`inherit");
    let child_recorder = child.recorder();
    let (emit, _unused_feed) = crate::bus::dummy_emitter();
    let setup = child.ral(
        &child.mailbox.life().child(),
        "headless-short-drain\nlet h = spawn \"held helper worker\" { headless-held-worker }",
        5,
        &emit,
    );
    assert!(setup.text.contains("EXIT: 0"), "{}", setup.text);
    working
        .recv_timeout(WAIT)
        .expect("helper worker really started");
    let helper = testkit::launch_child(&mut root, child, None, &emit);
    let fleet = root.fleet.clone();
    let host = testkit::host(root);
    run_frontend(&host, OutputFormat::Json).expect("root deliberately replied");
    assert!(
        helper.is_live(),
        "the report does not end the helper's life"
    );
    let failures = host.end();
    release.send(()).unwrap();
    assert!(!helper.is_live());
    assert!(fleet.is_empty(), "end joins both owner and helper");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("agent 'helper' ended without certified engine teardown"),
        "{failures:?}"
    );
    assert!(
        failures[0].contains("1 ral worker thread(s)"),
        "{failures:?}"
    );
    let (_, commits) = child_recorder.since(None).unwrap();
    let recorded: Vec<_> = commits
        .map(Result::unwrap)
        .flat_map(Commit::into_records)
        .filter_map(|record| match record.into_value() {
            Record::Forensic(Forensic::TeardownFailed { failure }) => Some(failure),
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded, failures,
        "the joiner propagates rather than rerecords the failure"
    );
}
